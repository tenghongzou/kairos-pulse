//! Agent 註冊表：記錄每個 agent_id 的連線與存活狀態。
//!
//! 使用 `std::sync::Mutex`，所有方法都是同步的，鎖不會跨 `.await`。
//! 時間由呼叫端傳入，方便單元測試。
//!
//! 持鎖時間（R2-K15）：除了主表之外另維護兩個索引，讓常見操作不必掃描整張表：
//! - `connected`：連線工作仍在的 agent（數量不超過 `server.max_connections`）；sweep 判定離線、
//!   現役連線清單與線上數都只走訪它。
//! - `offline`：連線已結束的 agent，依離線時間排序；狀態表已滿時淘汰最舊者、sweep 清除離線超過
//!   24 小時者都是 O(log n)。
//!
//! 字串欄位以 `Arc<str>` 保存，snapshot 在鎖內只複製參考計數，格式化與排序都在鎖外進行。
//!
//! 狀態變化（上線、離線）另以 [`HostChange`] 廣播（[`Registry::subscribe`]），主控台 API 的 SSE
//! 由此推送（`kairos/API.md` 第 6 節）。在鎖內送出：broadcast 不會阻塞，沒有接收端時直接丟棄。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot};
use tokio_rustls::rustls::pki_types::CertificateDer;

use crate::bus::{DisconnectReason, EventBus, EventKind, peer_ip};
use crate::protocol::HostInfo;
use crate::storage::fmt_time;
use crate::tls::ChainRejection;

/// 離線超過此時間的 agent 會在 sweep 時從註冊表移除
pub const OFFLINE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// 狀態表筆數上限的預設值（與 `limits.max_agents` 的預設相同）
pub const DEFAULT_MAX_AGENTS: usize = 10000;

/// 狀態變化廣播的緩衝（接收端落後超過這麼多則時收到 `Lagged`，SSE 改送 `resync`）
pub const HOST_CHANGE_CAPACITY: usize = 4096;

/// 收到的 durable report 的取樣時間比收到的時間早這麼多以上，視為補送的資料
pub const BACKFILL_LAG_MS: i64 = 60_000;

/// 最近一次補送在這麼久之內，視為「補送中」
pub const BACKFILL_ACTIVE: Duration = Duration::from_secs(60);

/// 最近一次離線的原因（只有 Kairos 這次啟動後發生的才知道；`kairos/API.md` 第 5.2 節）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineReason {
    /// 超過離線門檻沒有合法訊息（sweep）
    Timeout,
    /// 連線關閉（agent 關閉、網路中斷、伺服器關閉連線）
    ConnectionClosed,
    /// Kairos 關機
    ServerShutdown,
    /// 連線中重新驗證客戶端憑證時被拒（撤銷、過期、不再受信任）；握手時被拒的另由
    /// `tls::rejections` 記錄
    CertificateRejected,
    /// report 的寫入結果未知，Kairos 關閉了連線（設計包 §24 #2）
    WriteUnknown,
}

impl OfflineReason {
    /// 主機 API、SSE、Webhook 與 `/metrics` 共用的名稱（0.7 起統一為事件匯流排的名稱，
    /// [`crate::bus::DisconnectReason::as_str`]；設計包 `decisions/INTEGRATION.md` §28.5 #4）
    pub fn as_str(self) -> &'static str {
        match self {
            OfflineReason::Timeout => "idle_timeout",
            OfflineReason::ConnectionClosed => "agent_closed",
            OfflineReason::ServerShutdown => "server_shutdown",
            OfflineReason::CertificateRejected => "certificate_rejected",
            OfflineReason::WriteUnknown => "write_unknown",
        }
    }
}

/// 0.4～0.6 的主機 API 使用的舊名稱（`timeout`、`connection_closed`）：0.7 的過渡期在回應另帶
/// `offline_reason_legacy`，查詢參數也接受舊名稱（`kairos/API.md` 第 5.2 節）；其餘名稱相同
pub fn legacy_offline_reason(name: &str) -> &str {
    match name {
        "idle_timeout" => "timeout",
        "agent_closed" => "connection_closed",
        other => other,
    }
}

/// 舊名稱 → 0.7 的名稱（查詢參數的相容；不認得的值原樣回傳）
pub fn normalize_offline_reason(name: &str) -> &str {
    match name {
        "timeout" => "idle_timeout",
        "connection_closed" => "agent_closed",
        other => other,
    }
}

/// 狀態變化的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostChangeKind {
    /// 上線、重新上線、離線後恢復
    Online,
    Offline(OfflineReason),
}

/// 一次狀態變化（[`Registry::subscribe`]）
#[derive(Debug, Clone)]
pub struct HostChange {
    pub agent_id: Arc<str>,
    pub hostname: Arc<str>,
    pub display_name: Option<String>,
    pub peer: Arc<str>,
    pub kind: HostChangeKind,
    pub at: DateTime<Utc>,
}

/// 最近一次收到補送資料
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backfill {
    pub at: Instant,
    pub at_wall: DateTime<Utc>,
    /// 收到的時間 − 取樣時間（毫秒）
    pub lag_ms: i64,
}

/// 主控台 API 用的即時狀態（[`Registry::live_view`]）：字串只複製參考計數
#[derive(Debug, Clone)]
pub struct LiveAgent {
    pub agent_id: Arc<str>,
    pub hostname: Arc<str>,
    pub agent_version: Arc<str>,
    pub proto: u32,
    pub host: Option<Arc<HostInfo>>,
    pub peer: Arc<str>,
    pub online: bool,
    /// Kairos 這次啟動後是否連線過（`false`：由主機清冊載回、尚未連線）
    pub seen_since_start: bool,
    pub connected_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub offline_after: Duration,
    pub last_report: Option<Arc<LastReport>>,
    /// 最近一次連線是否協商了 durable（尚未連線時為 `None`）
    pub durable: Option<bool>,
    /// 最近一次連線是否協商了 `inventory`、`processes`（尚未連線時為 `None`）
    pub inventory: Option<bool>,
    pub processes: Option<bool>,
    /// 這次啟動後最近一次保存的資產摘要
    pub assets: Option<Arc<AssetSummary>>,
    /// 這次啟動後最近一次資產被拒（`KRS-W2108`）的時間與原因
    pub inventory_rejected: Option<(DateTime<Utc>, Arc<str>)>,
    pub backfill: Option<Backfill>,
    pub offline_reason: Option<OfflineReason>,
    /// 離線多久了（線上時為 `None`；由主機清冊載回的主機以載回的時間起算）
    pub offline_for: Option<Duration>,
}

impl LiveAgent {
    /// 目前是否正在補送（線上，且最近 [`BACKFILL_ACTIVE`] 內收到補送的資料）
    pub fn replaying(&self, now: Instant) -> bool {
        self.online
            && self
                .backfill
                .is_some_and(|b| now.saturating_duration_since(b.at) <= BACKFILL_ACTIVE)
    }
}

/// 全域連線編號
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// 取得新的連線編號（每條 TCP 連線一個，從 1 起遞增）
pub fn next_conn_id() -> u64 {
    NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed)
}

/// hello 帶來的 agent 資訊
#[derive(Debug, Clone)]
pub struct HelloInfo {
    pub agent_id: String,
    pub hostname: String,
    pub agent_version: String,
    /// 協商出的協定版本
    pub proto: u32,
    /// v2 hello 的主機資訊（協商為 v1、或 client 沒有送任何主機資訊時為 `None`）
    pub host: Option<Arc<HostInfo>>,
    pub peer: String,
    pub offline_after: Duration,
    /// 握手時取得的客戶端憑證鏈（[0] 為 end-entity）；未使用 mTLS 時為 None
    pub client_chain: Option<ClientChain>,
}

/// 客戶端憑證鏈（共用，避免重新驗證時複製 DER）
pub type ClientChain = Arc<[CertificateDer<'static>]>;

/// 要求連線結束的原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KickReason {
    /// 同 agent_id 有新連線
    Superseded,
    /// 重新載入 CA/CRL 後或定期檢查時，客戶端憑證已被撤銷、過期、不再受信任，或無法判定撤銷狀態
    CertificateRejected(ChainRejection),
}

/// 一條現役連線（用於重新驗證客戶端憑證）
#[derive(Debug, Clone)]
pub struct LiveConnection {
    pub agent_id: String,
    pub conn_id: u64,
    pub client_chain: Option<ClientChain>,
}

/// 最近一次合法的 report（也存在 SQLite 主機清冊的 `hosts.last_report`，重新啟動時讀回）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastReport {
    pub recv_at: String,
    pub seq: u64,
    pub ts: String,
    /// v1 report 一定有值；v2 的指標 report 取自 `system.cpu.usage`，沒有時不輸出
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_usage: Option<f32>,
    /// MB；v2 的指標 report 由 `system.memory.total`（bytes）換算，沒有時不輸出
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_memory: Option<u64>,
    /// MB；v2 的指標 report 由 `system.memory.used`（bytes）換算，沒有時不輸出
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_memory: Option<u64>,
    /// v2 的指標 report 中的指標數（v1 不輸出）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<usize>,
    /// 0.8.0 起的指標（METRICS.md 第 11.1 節：告警與主控台的「目前數值」）；舊版 Pulse 沒有的欄位不輸出
    #[serde(flatten, default)]
    pub detail: ReportDetail,
}

/// 最近一次 report 中 F4 指標的摘要（[`LastReport::detail`]）。計數器另保存原始累計值，下一筆 report
/// 到達時以 [`kairos_proto::counter_rate`] 算出速率（[`Registry::set_last_report`]）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportDetail {
    /// `system.uptime`（秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    /// `system.memory.available`（bytes）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_available: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_1m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_15m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u64>,
    /// 每個掛載點
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filesystems: Vec<FsLast>,
    /// 每個介面
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<IoLast>,
    /// 每個磁碟 I/O 裝置
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<IoLast>,
}

/// 一個掛載點的容量（bytes）與使用率
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FsLast {
    pub mountpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fstype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<u64>,
    /// 使用率（%）：Pulse 送的 `system.filesystem.usage`，沒有時為推算值（`estimated`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<f64>,
    /// `usage` 是由 total 與 available 推算的（0.2.0～0.7.0 的 Pulse；METRICS.md 第 9.2 節）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub estimated: bool,
}

/// 一個介面或磁碟裝置的計數器（累計 bytes）與每秒速率（bytes/s）：網路為接收／傳送，磁碟為讀取／寫入
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IoLast {
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_rate: Option<f64>,
}

impl ReportDetail {
    /// 使用率最高的掛載點
    pub fn top_filesystem(&self) -> Option<&FsLast> {
        self.filesystems
            .iter()
            .filter(|f| f.usage.is_some())
            .max_by(|a, b| {
                a.usage
                    .partial_cmp(&b.usage)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// 置換空間使用率（%）；總量為 0 時沒有值
    pub fn swap_percent(&self) -> Option<f64> {
        match (self.swap_used, self.swap_total) {
            (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
            _ => None,
        }
    }
}

/// 以上一筆 report 算出計數器的速率（取樣時間為 `ts`，RFC 3339；無法解析時不計算）
fn carry_rates(prev: &LastReport, new: &mut LastReport) {
    let parse = |ts: &str| {
        DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|t| t.timestamp_millis())
    };
    let (Some(p_ts), Some(n_ts)) = (parse(&prev.ts), parse(&new.ts)) else {
        return;
    };
    let p_up = prev.detail.uptime_secs;
    let n_up = new.detail.uptime_secs;
    let rate = |p: Option<u64>, n: Option<u64>| -> Option<f64> {
        kairos_proto::counter_rate(
            kairos_proto::CounterSample {
                value: p?,
                ts_ms: p_ts,
                uptime_secs: p_up,
            },
            kairos_proto::CounterSample {
                value: n?,
                ts_ms: n_ts,
                uptime_secs: n_up,
            },
        )
    };
    let fill = |prev: &[IoLast], new: &mut [IoLast]| {
        for io in new.iter_mut() {
            if let Some(p) = prev.iter().find(|p| p.device == io.device) {
                io.rx_rate = rate(p.rx, io.rx);
                io.tx_rate = rate(p.tx, io.tx);
            }
        }
    };
    fill(&prev.detail.network, &mut new.detail.network);
    fill(&prev.detail.disks, &mut new.detail.disks);
}

/// `new` 是否代表重新開機：兩筆都有 uptime、`new` 的取樣時間較晚且 uptime 變小。回傳 (之前, 之後)
fn rebooted(prev: &LastReport, new: &LastReport) -> Option<(u64, u64)> {
    let parse = |ts: &str| DateTime::parse_from_rfc3339(ts).ok();
    let (p, n) = (prev.detail.uptime_secs?, new.detail.uptime_secs?);
    let later = matches!((parse(&prev.ts), parse(&new.ts)), (Some(a), Some(b)) if b > a);
    (later && n < p).then_some((p, n))
}

/// 以新的 report 取代 last_report：以上一筆算出計數器的速率，並判斷是否重新開機（回傳要發布的事件）
fn replace_last_report(
    agent_id: &str,
    state: &mut AgentState,
    mut report: LastReport,
) -> Option<EventKind> {
    let mut reboot = None;
    if let Some(prev) = &state.last_report {
        carry_rates(prev, &mut report);
        if let Some((before, now)) = rebooted(prev, &report) {
            let boot_time = DateTime::parse_from_rfc3339(&report.ts).ok().map(|t| {
                fmt_time(
                    t.with_timezone(&Utc)
                        - chrono::Duration::seconds(i64::try_from(now).unwrap_or(0)),
                )
            });
            reboot = Some(EventKind::HostRebooted {
                agent_id: agent_id.to_string(),
                hostname: state.hostname.to_string(),
                uptime_secs: now,
                previous_uptime_secs: before,
                boot_time,
            });
        }
    }
    state.last_report = Some(Arc::new(report));
    reboot
}

/// 主機資產的摘要（`inventory` 保存後；告警的每核心負載）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetSummary {
    pub logical_cores: Option<u32>,
    pub os_family: Option<String>,
}

#[derive(Debug)]
pub struct AgentState {
    pub conn_id: u64,
    pub peer: Arc<str>,
    pub hostname: Arc<str>,
    pub agent_version: Arc<str>,
    pub proto: u32,
    pub host: Option<Arc<HostInfo>>,
    pub online: bool,
    pub offline_after: Duration,
    pub last_seen: Instant,
    pub last_seen_wall: DateTime<Utc>,
    pub connected_at: DateTime<Utc>,
    /// 被標記為離線的時間（線上時為 `None`），用於淘汰與 24 小時清除
    pub offline_since: Option<Instant>,
    pub last_report: Option<Arc<LastReport>>,
    /// 握手時的客戶端憑證鏈；只在連線期間保留（重新驗證用），斷線時清除
    pub client_chain: Option<ClientChain>,
    /// 連線仍在且尚未被通知結束時為 Some；用來通知該連線結束
    pub kick: Option<oneshot::Sender<KickReason>>,
    /// 連線工作是否仍在（從 register 到 disconnect）。被 kick 後、連線尚未結束前 `kick` 已是
    /// `None`，但這裡仍為 `true`
    pub connected: bool,
    /// 在離線索引中的 key（連線已結束時為 `Some`）
    offline_key: Option<OfflineKey>,
    /// 連線是否協商了 durable（[`Registry::set_durable`]；由清冊載回、尚未連線時為 `None`）
    pub durable: Option<bool>,
    /// 連線是否協商了 `inventory`、`processes`（[`Registry::set_features`]）
    pub inventory: Option<bool>,
    pub processes: Option<bool>,
    /// 最近一次保存的資產摘要（[`Registry::set_assets`]）
    pub assets: Option<Arc<AssetSummary>>,
    /// 最近一次資產被拒（`KRS-W2108`）的時間與原因（[`Registry::set_inventory_rejected`]）
    pub inventory_rejected: Option<(DateTime<Utc>, Arc<str>)>,
    /// 最近一次收到補送的資料（[`Registry::note_backfill`]）
    pub backfill: Option<Backfill>,
    /// 最近一次離線的原因（線上時保留上一次的值，供詳情顯示）
    pub offline_reason: Option<OfflineReason>,
    /// 連線因憑證被拒而被要求結束時記下，連線結束時作為離線原因
    kick_reason: Option<OfflineReason>,
}

/// 離線索引的 key：(離線時間, 序號)；序號讓同一時間離線的多個 agent 不會衝突
type OfflineKey = (Instant, u64);

/// 從 SQLite 主機清冊載回的 agent（啟動時；審計 A6），見 [`Registry::restore`]
#[derive(Debug, Clone, PartialEq)]
pub struct RestoredAgent {
    pub agent_id: String,
    pub hostname: String,
    pub agent_version: String,
    pub proto: u32,
    pub host: Option<HostInfo>,
    pub peer: String,
    pub connected_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub offline_after: Duration,
    pub last_report: Option<LastReport>,
}

/// 註冊結果的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterKind {
    /// 第一次出現的 agent（或已被清除後再出現）
    New,
    /// 先前離線，現在重新上線
    Reconnected,
    /// 舊連線仍在（含已被通知結束、但尚未關閉的連線），已改由新連線取代
    Superseded { previous_conn_id: u64 },
}

/// 為了騰出空間而從狀態表淘汰的離線 agent
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evicted {
    pub agent_id: String,
    pub hostname: String,
    pub offline_for: Duration,
}

#[derive(Debug)]
pub struct Registration {
    pub kind: RegisterKind,
    /// 被新連線取代時會收到通知
    pub kick: oneshot::Receiver<KickReason>,
    /// 狀態表已滿時，為了這次註冊而淘汰的離線 agent
    pub evicted: Option<Evicted>,
    /// 同一個 agent_id 上次登記的主機指紋與這次不同時，為上次的指紋（設計包的身分衝突偵測：
    /// 例如複製的 VM 沿用了 `agent.id`）。任一次沒有指紋時不比較
    pub previous_fingerprint: Option<String>,
}

/// 狀態表已達上限，且沒有可淘汰的離線 agent。
///
/// 防禦性檢查：設定要求 `limits.max_agents >= server.max_connections`，而連線工作仍在的 agent
/// 不超過同時連線數（含這次註冊的連線），因此表滿時一定有連線已結束、可淘汰的 agent，
/// 正常情況下走不到這裡（R2-K22）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryFull {
    pub max_agents: usize,
}

/// 以連線身分更新註冊表的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Touch {
    /// 本連線是現役連線，已更新
    Updated,
    /// 本連線是現役連線，且 agent 先前被 sweep 判定離線，現已恢復上線
    Revived,
    /// 本連線已不是該 agent 的現役連線（已被取代、已結束或已被清除），未做任何更新
    Stale,
}

/// sweep 時新判定為離線的 agent
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WentOffline {
    pub agent_id: String,
    pub hostname: String,
    pub silent_for: Duration,
}

/// sweep 時因離線過久而移除的 agent
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub agent_id: String,
    pub hostname: String,
    pub offline_for: Duration,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepResult {
    pub went_offline: Vec<WentOffline>,
    pub removed: Vec<Removed>,
}

/// agents.json 中的單一 agent
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub hostname: String,
    pub agent_version: String,
    /// 協商出的協定版本
    pub proto: u32,
    /// v2 hello 的主機資訊（`host_fingerprint`、`display_name`、`tags`、`report_interval_ms`、`os`），
    /// 只輸出有值的欄位
    #[serde(flatten)]
    pub host: Option<HostInfo>,
    pub peer: String,
    pub online: bool,
    pub conn_id: u64,
    pub connected_at: String,
    pub last_seen: String,
    pub offline_after_ms: u64,
    pub last_report: Option<LastReport>,
}

/// agents.json 的完整內容
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegistrySnapshot {
    pub generated_at: String,
    pub online: usize,
    pub total: usize,
    pub agents: Vec<AgentSnapshot>,
}

/// 持鎖期間只複製參考計數與數值欄位，字串複製、時間格式化與排序在鎖外進行
struct RawAgent {
    agent_id: Arc<str>,
    hostname: Arc<str>,
    agent_version: Arc<str>,
    proto: u32,
    host: Option<Arc<HostInfo>>,
    peer: Arc<str>,
    online: bool,
    conn_id: u64,
    connected_at: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    offline_after: Duration,
    last_report: Option<Arc<LastReport>>,
}

#[derive(Debug, Default)]
struct Inner {
    agents: HashMap<Arc<str>, AgentState>,
    /// 連線工作仍在的 agent
    connected: HashSet<Arc<str>>,
    /// 連線已結束的 agent，依離線時間排序
    offline: BTreeMap<OfflineKey, Arc<str>>,
    next_offline_seq: u64,
}

impl Inner {
    /// 連線結束：標記離線、釋放只在連線期間需要的資源，並移入離線索引。
    /// `reason` 為仍在線上時的離線原因（連線因憑證被拒而被要求結束時改用 `CertificateRejected`）
    fn mark_disconnected(&mut self, agent_id: &Arc<str>, now: Instant, reason: OfflineReason) {
        let seq = self.next_offline_seq;
        let Some(state) = self.agents.get_mut(agent_id) else {
            return;
        };
        state.kick = None;
        state.connected = false;
        state.client_chain = None;
        if state.online {
            state.offline_reason = Some(state.kick_reason.take().unwrap_or(reason));
        }
        state.kick_reason = None;
        state.online = false;
        let since = *state.offline_since.get_or_insert(now);
        if state.offline_key.is_none() {
            let key = (since, seq);
            state.offline_key = Some(key);
            self.offline.insert(key, agent_id.clone());
            self.next_offline_seq += 1;
        }
        self.connected.remove(agent_id);
    }

    /// 從主表與兩個索引移除
    fn remove(&mut self, agent_id: &str) -> Option<AgentState> {
        let state = self.agents.remove(agent_id)?;
        if let Some(key) = state.offline_key {
            self.offline.remove(&key);
        }
        self.connected.remove(agent_id);
        Some(state)
    }

    /// 移除連線已結束、離線最久的 agent；沒有可淘汰者時回傳 `None`
    fn evict_oldest_offline(&mut self, now: Instant) -> Option<Evicted> {
        let (_, agent_id) = self.offline.first_key_value()?;
        let agent_id = agent_id.clone();
        let state = self.remove(&agent_id)?;
        Some(Evicted {
            agent_id: agent_id.to_string(),
            hostname: state.hostname.to_string(),
            offline_for: state
                .offline_since
                .map(|since| now.saturating_duration_since(since))
                .unwrap_or_default(),
        })
    }
}

#[derive(Debug)]
pub struct Registry {
    inner: Mutex<Inner>,
    /// 狀態表最多保留的 agent 數（`limits.max_agents`）
    max_agents: usize,
    /// 狀態變化的廣播（[`Registry::subscribe`]）
    events: broadcast::Sender<HostChange>,
    /// Kairos 內部的事件匯流排（[`crate::bus`]）：上線、離線、被取代也發布到這裡
    bus: Arc<EventBus>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_max_agents(DEFAULT_MAX_AGENTS)
    }
}

impl Registry {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_agents(max_agents: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_agents: max_agents.max(1),
            events: broadcast::channel(HOST_CHANGE_CAPACITY).0,
            bus: EventBus::new(),
        }
    }

    /// 事件匯流排（狀態表建立時一併建立；其他元件經由狀態表取得同一個匯流排）
    pub fn bus(&self) -> Arc<EventBus> {
        self.bus.clone()
    }

    /// 訂閱之後的狀態變化（上線、離線）
    pub fn subscribe(&self) -> broadcast::Receiver<HostChange> {
        self.events.subscribe()
    }

    /// 送出一則狀態變化（持鎖中呼叫；沒有接收端時丟棄）
    fn emit(
        &self,
        agent_id: &Arc<str>,
        state: &AgentState,
        kind: HostChangeKind,
        at: DateTime<Utc>,
    ) {
        let ip = peer_ip(&state.peer);
        self.bus.publish_at(
            at,
            match kind {
                HostChangeKind::Online => EventKind::HostOnline {
                    agent_id: agent_id.clone(),
                    hostname: state.hostname.clone(),
                    ip,
                },
                HostChangeKind::Offline(reason) => EventKind::HostOffline {
                    agent_id: agent_id.clone(),
                    hostname: state.hostname.clone(),
                    ip,
                    reason: DisconnectReason::from_offline(reason),
                },
            },
        );
        if self.events.receiver_count() == 0 {
            return;
        }
        let _ = self.events.send(HostChange {
            agent_id: agent_id.clone(),
            hostname: state.hostname.clone(),
            display_name: state.host.as_ref().and_then(|h| h.display_name.clone()),
            peer: state.peer.clone(),
            kind,
            at,
        });
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // 持鎖期間不會 panic（只有簡單的欄位更新），即使被污染也沿用內部資料
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 以 hello 登記連線。若同 agent_id 已有連線，通知舊連線被取代。
    ///
    /// 新的 agent_id 遇到狀態表已滿時，先淘汰離線最久（且連線已結束）的 agent；
    /// 沒有可淘汰者時回 [`RegistryFull`]，不做任何變更。
    pub fn register(
        &self,
        conn_id: u64,
        info: HelloInfo,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Result<Registration, RegistryFull> {
        let mut inner = self.lock();
        let mut evicted = None;
        if !inner.agents.contains_key(info.agent_id.as_str())
            && inner.agents.len() >= self.max_agents
        {
            // 淘汰由連線發布 `RegistryEvicted` 到匯流排（crate::metrics::watch_bus 計數）；已滿在這裡計數
            match inner.evict_oldest_offline(now) {
                Some(e) => {
                    evicted = Some(e);
                }
                None => {
                    crate::metrics::global().registry_full.inc();
                    return Err(RegistryFull {
                        max_agents: self.max_agents,
                    });
                }
            }
        }
        let (tx, rx) = oneshot::channel();
        let fingerprint =
            |host: &Option<Arc<HostInfo>>| host.as_ref().and_then(|h| h.host_fingerprint.clone());
        let mut previous_fingerprint = None;
        let mut was_online = false;
        let mut previous_reason = None;
        let mut previous_assets = None;
        let mut previous_rejected = None;
        let mut superseded_peer = None;
        let (kind, last_report, key) = match inner.remove(&info.agent_id) {
            None => (RegisterKind::New, None, Arc::<str>::from(info.agent_id)),
            Some(mut prev) => {
                was_online = prev.online;
                previous_reason = prev.offline_reason;
                previous_assets = prev.assets.take();
                previous_rejected = prev.inventory_rejected.take();
                if let (Some(old), Some(new)) = (fingerprint(&prev.host), fingerprint(&info.host))
                    && old != new
                {
                    previous_fingerprint = Some(old);
                }
                let previous_conn_id = prev.conn_id;
                let kind = match prev.kick.take() {
                    // 舊連線仍在：通知它被取代。接收端已結束時 send 會失敗，可忽略。
                    Some(kick) => {
                        let _ = kick.send(KickReason::Superseded);
                        superseded_peer = Some(prev.peer.clone());
                        RegisterKind::Superseded { previous_conn_id }
                    }
                    // 舊連線已被通知結束（例如憑證被拒）但尚未關閉：同樣是取代，只是不必再通知
                    None if prev.connected => {
                        superseded_peer = Some(prev.peer.clone());
                        RegisterKind::Superseded { previous_conn_id }
                    }
                    None => RegisterKind::Reconnected,
                };
                (
                    kind,
                    prev.last_report.take(),
                    Arc::<str>::from(info.agent_id),
                )
            }
        };
        inner.connected.insert(key.clone());
        let event_key = key.clone();
        inner.agents.insert(
            key,
            AgentState {
                conn_id,
                peer: info.peer.into(),
                hostname: info.hostname.into(),
                agent_version: info.agent_version.into(),
                proto: info.proto,
                host: info.host,
                online: true,
                offline_after: info.offline_after,
                last_seen: now,
                last_seen_wall: now_wall,
                connected_at: now_wall,
                offline_since: None,
                last_report,
                client_chain: info.client_chain,
                kick: Some(tx),
                connected: true,
                offline_key: None,
                durable: None,
                inventory: None,
                processes: None,
                assets: previous_assets,
                inventory_rejected: previous_rejected,
                backfill: None,
                offline_reason: previous_reason,
                kick_reason: None,
            },
        );
        if !was_online && let Some(state) = inner.agents.get(&event_key) {
            self.emit(&event_key, state, HostChangeKind::Online, now_wall);
        }
        if let (Some(previous), Some(state)) = (superseded_peer, inner.agents.get(&event_key)) {
            self.bus.publish_at(
                now_wall,
                EventKind::HostSuperseded {
                    agent_id: event_key.clone(),
                    previous_ip: peer_ip(&previous),
                    ip: peer_ip(&state.peer),
                },
            );
        }
        Ok(Registration {
            kind,
            kick: rx,
            evicted,
            previous_fingerprint,
        })
    }

    /// 啟動時把主機清冊中離線未超過 [`OFFLINE_RETENTION`] 的 agent 以「離線」狀態放回狀態表
    /// （審計 A6）：重新啟動後 `agents.json` 不再是空的，之後的 hello 記為「重新上線」，
    /// 主機指紋的比對也延續到重新啟動之後。離線時間以清冊的 `last_seen` 起算（24 小時後照常由
    /// sweep 移除）；最多放回狀態表上限的筆數（最近出現的優先），已在表中的 agent 不覆寫。
    /// 回傳放回的筆數。
    pub fn restore(
        &self,
        mut agents: Vec<RestoredAgent>,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> usize {
        agents.sort_by_key(|a| std::cmp::Reverse(a.last_seen));
        let mut inner = self.lock();
        let mut restored = 0;
        for a in agents {
            if inner.agents.len() >= self.max_agents {
                break;
            }
            let offline_for = (now_wall - a.last_seen).to_std().unwrap_or_default();
            if offline_for > OFFLINE_RETENTION || inner.agents.contains_key(a.agent_id.as_str()) {
                continue;
            }
            // 開機不久時單調時鐘可能無法往回推，改以現在起算（最多讓移除晚一點）
            let since = now.checked_sub(offline_for).unwrap_or(now);
            let key = (since, inner.next_offline_seq);
            inner.next_offline_seq += 1;
            let agent_id: Arc<str> = Arc::from(a.agent_id);
            inner.offline.insert(key, agent_id.clone());
            inner.agents.insert(
                agent_id,
                AgentState {
                    // 連線編號從 1 開始，0 不會與任何連線相符
                    conn_id: 0,
                    peer: a.peer.into(),
                    hostname: a.hostname.into(),
                    agent_version: a.agent_version.into(),
                    proto: a.proto,
                    host: a.host.map(Arc::new),
                    online: false,
                    offline_after: a.offline_after,
                    last_seen: since,
                    last_seen_wall: a.last_seen,
                    connected_at: a.connected_at,
                    offline_since: Some(since),
                    last_report: a.last_report.map(Arc::new),
                    client_chain: None,
                    kick: None,
                    connected: false,
                    offline_key: Some(key),
                    durable: None,
                    inventory: None,
                    processes: None,
                    assets: None,
                    inventory_rejected: None,
                    backfill: None,
                    offline_reason: None,
                    kick_reason: None,
                },
            );
            restored += 1;
        }
        restored
    }

    /// 列出仍有連線（且尚未被通知結束）的 agent 與其客戶端憑證鏈
    pub fn live_connections(&self) -> Vec<LiveConnection> {
        let inner = self.lock();
        inner
            .connected
            .iter()
            .filter_map(|agent_id| {
                let s = inner.agents.get(agent_id)?;
                s.kick.is_some().then(|| LiveConnection {
                    agent_id: agent_id.to_string(),
                    conn_id: s.conn_id,
                    client_chain: s.client_chain.clone(),
                })
            })
            .collect()
    }

    /// 通知指定連線結束；conn_id 不符或連線已結束時回傳 `false`
    pub fn kick(&self, agent_id: &str, conn_id: u64, reason: KickReason) -> bool {
        let mut inner = self.lock();
        match inner.agents.get_mut(agent_id) {
            Some(state) if state.conn_id == conn_id => match state.kick.take() {
                Some(kick) => {
                    if matches!(reason, KickReason::CertificateRejected(_)) {
                        state.kick_reason = Some(OfflineReason::CertificateRejected);
                    }
                    kick.send(reason).is_ok()
                }
                None => false,
            },
            _ => false,
        }
    }

    /// 連線即將由 Kairos 關閉時記下原因（例如 report 寫入結果未知），之後的離線以這個原因記錄；
    /// conn_id 不符或已離線時不更新
    pub fn set_close_reason(&self, agent_id: &str, conn_id: u64, reason: OfflineReason) {
        let mut inner = self.lock();
        if let Some(state) = inner.agents.get_mut(agent_id)
            && state.conn_id == conn_id
            && state.online
            && state.kick_reason.is_none()
        {
            state.kick_reason = Some(reason);
        }
    }

    /// hello 通過後記錄這條連線是否協商了 durable（A5）；conn_id 不符時不更新
    pub fn set_durable(&self, agent_id: &str, conn_id: u64, durable: bool) {
        let mut inner = self.lock();
        if let Some(state) = inner.agents.get_mut(agent_id)
            && state.conn_id == conn_id
        {
            state.durable = Some(durable);
        }
    }

    /// hello 通過後記錄這條連線是否協商了 `inventory`、`processes`（METRICS.md）；conn_id 不符時不更新
    pub fn set_features(&self, agent_id: &str, conn_id: u64, inventory: bool, processes: bool) {
        let mut inner = self.lock();
        if let Some(state) = inner.agents.get_mut(agent_id)
            && state.conn_id == conn_id
        {
            state.inventory = Some(inventory);
            state.processes = Some(processes);
        }
    }

    /// 資產保存後記錄摘要（告警的每核心負載）；conn_id 不符時不更新
    pub fn set_assets(&self, agent_id: &str, conn_id: u64, assets: AssetSummary) {
        let mut inner = self.lock();
        if let Some(state) = inner.agents.get_mut(agent_id)
            && state.conn_id == conn_id
        {
            state.assets = Some(Arc::new(assets));
            // 之後的資產已保存：不再顯示「最近一次被拒」
            state.inventory_rejected = None;
        }
    }

    /// 資產被拒（`KRS-W2108`）：記下時間與原因（主控台 S05 的提示）；conn_id 不符時不更新
    pub fn set_inventory_rejected(&self, agent_id: &str, conn_id: u64, reason: &str) {
        let mut inner = self.lock();
        if let Some(state) = inner.agents.get_mut(agent_id)
            && state.conn_id == conn_id
        {
            state.inventory_rejected = Some((Utc::now(), Arc::from(reason)));
        }
    }

    /// 收到補送的資料（durable report 的取樣時間比收到的時間早 [`BACKFILL_LAG_MS`] 以上）
    pub fn note_backfill(
        &self,
        agent_id: &str,
        conn_id: u64,
        lag_ms: i64,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) {
        let mut inner = self.lock();
        if let Some(state) = inner.agents.get_mut(agent_id)
            && state.conn_id == conn_id
        {
            state.backfill = Some(Backfill {
                at: now,
                at_wall: now_wall,
                lag_ms,
            });
        }
    }

    /// 主控台 API 用：所有 agent 的即時狀態（持鎖期間只複製參考計數）
    pub fn live_view(&self) -> Vec<LiveAgent> {
        let now = Instant::now();
        let inner = self.lock();
        inner
            .agents
            .iter()
            .map(|(id, s)| Self::live_of(id, s, now))
            .collect()
    }

    /// 主控台 API 用：一台 agent 的即時狀態
    pub fn live_get(&self, agent_id: &str) -> Option<LiveAgent> {
        let now = Instant::now();
        let inner = self.lock();
        inner
            .agents
            .get_key_value(agent_id)
            .map(|(id, s)| Self::live_of(id, s, now))
    }

    /// 刪除主機資料（`kairos/PRIVACY.md` 第 4 節）：從狀態表移除，之後不再出現在 `agents.json`、清冊同步
    /// 與主控台。連線仍在時不移除、回 `Err(())`；不在狀態表時回 `Ok(false)`
    pub fn forget(&self, agent_id: &str) -> Result<bool, ()> {
        let mut inner = self.lock();
        if inner.connected.contains(agent_id) {
            return Err(());
        }
        Ok(inner.remove(agent_id).is_some())
    }

    /// 這台 agent 的連線是否仍在
    pub fn is_connected(&self, agent_id: &str) -> bool {
        self.lock().connected.contains(agent_id)
    }

    /// 狀態表中的筆數與連線工作仍在的 agent 數
    pub fn sizes(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.agents.len(), inner.connected.len())
    }

    /// 狀態表的上限（`limits.max_agents`）
    pub fn max_agents(&self) -> usize {
        self.max_agents
    }

    fn live_of(id: &Arc<str>, s: &AgentState, now: Instant) -> LiveAgent {
        LiveAgent {
            agent_id: id.clone(),
            hostname: s.hostname.clone(),
            agent_version: s.agent_version.clone(),
            proto: s.proto,
            host: s.host.clone(),
            peer: s.peer.clone(),
            online: s.online,
            seen_since_start: s.conn_id != 0,
            connected_at: s.connected_at,
            last_seen: s.last_seen_wall,
            offline_after: s.offline_after,
            last_report: s.last_report.clone(),
            durable: s.durable,
            inventory: s.inventory,
            processes: s.processes,
            assets: s.assets.clone(),
            inventory_rejected: s.inventory_rejected.clone(),
            backfill: s.backfill,
            offline_reason: s.offline_reason,
            offline_for: if s.online {
                None
            } else {
                s.offline_since.map(|t| now.saturating_duration_since(t))
            },
        }
    }

    /// 收到合法訊息時更新 last_seen。conn_id 不符（已被取代或已被清除）或連線已結束時不做任何更新。
    pub fn touch(
        &self,
        agent_id: &str,
        conn_id: u64,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Touch {
        let mut inner = self.lock();
        let touch = match inner.agents.get_mut(agent_id) {
            Some(state) if state.conn_id == conn_id && state.connected => {
                state.last_seen = now;
                state.last_seen_wall = now_wall;
                let revived = !state.online;
                state.online = true;
                state.offline_since = None;
                if revived {
                    Touch::Revived
                } else {
                    Touch::Updated
                }
            }
            _ => Touch::Stale,
        };
        if touch == Touch::Revived
            && let Some((id, state)) = inner.agents.get_key_value(agent_id)
        {
            self.emit(id, state, HostChangeKind::Online, now_wall);
        }
        touch
    }

    /// report 已寫入資料檔並 flush 後才呼叫，記錄為最近一次 report，確保 agents.json 與
    /// `reports-YYYY-MM-DD.jsonl` 一致。conn_id 不符時不更新並回傳 `false`。
    pub fn set_last_report(&self, agent_id: &str, conn_id: u64, report: LastReport) -> bool {
        let mut inner = self.lock();
        let reboot = match inner.agents.get_mut(agent_id) {
            Some(state) if state.conn_id == conn_id => replace_last_report(agent_id, state, report),
            _ => return false,
        };
        drop(inner);
        self.publish_reboot(reboot);
        true
    }

    fn publish_reboot(&self, reboot: Option<EventKind>) {
        if let Some(kind) = reboot {
            self.bus.publish(kind);
        }
    }

    /// 同 [`Registry::set_last_report`]，但 `report.ts` 早於目前 last_report 的 `ts` 時不更新
    /// （durable 補送的舊資料不蓋過較新的狀態；A5）。任一方的 `ts` 不是 RFC 3339 時照常更新。
    /// 回傳是否更新
    pub fn set_last_report_unless_older(
        &self,
        agent_id: &str,
        conn_id: u64,
        report: LastReport,
    ) -> bool {
        let mut inner = self.lock();
        match inner.agents.get_mut(agent_id) {
            Some(state) if state.conn_id == conn_id => {
                let parse = |ts: &str| DateTime::parse_from_rfc3339(ts).ok();
                let older = state.last_report.as_ref().is_some_and(|current| {
                    matches!(
                        (parse(&report.ts), parse(&current.ts)),
                        (Some(new), Some(cur)) if new < cur
                    )
                });
                if !older {
                    let reboot = replace_last_report(agent_id, state, report);
                    drop(inner);
                    self.publish_reboot(reboot);
                }
                !older
            }
            _ => false,
        }
    }

    /// 關機時呼叫：把仍有連線的 agent 全部標為離線（例如連線工作逾時被中止，
    /// 來不及自行 disconnect）。回傳其中仍標記為線上的數量。
    pub fn mark_all_offline(&self, now: Instant) -> usize {
        let mut inner = self.lock();
        let ids: Vec<Arc<str>> = inner.connected.iter().cloned().collect();
        let mut count = 0;
        let now_wall = Utc::now();
        for agent_id in &ids {
            let was_online = inner.agents.get(agent_id).is_some_and(|s| s.online);
            inner.mark_disconnected(agent_id, now, OfflineReason::ServerShutdown);
            if was_online {
                count += 1;
                if let Some(state) = inner.agents.get(agent_id) {
                    self.emit(
                        agent_id,
                        state,
                        HostChangeKind::Offline(OfflineReason::ServerShutdown),
                        now_wall,
                    );
                }
            }
        }
        count
    }

    /// 連線結束。conn_id 不符（舊連線被取代後才結束）時忽略。
    /// 回傳 `true` 表示這次呼叫把 agent 從線上改為離線。
    pub fn disconnect(&self, agent_id: &str, conn_id: u64, now: Instant) -> bool {
        let mut inner = self.lock();
        let Some((key, state)) = inner.agents.get_key_value(agent_id) else {
            return false;
        };
        if state.conn_id != conn_id || !state.connected {
            return false;
        }
        let was_online = state.online;
        let key = key.clone();
        inner.mark_disconnected(&key, now, OfflineReason::ConnectionClosed);
        if was_online && let Some(state) = inner.agents.get(&key) {
            let reason = state
                .offline_reason
                .unwrap_or(OfflineReason::ConnectionClosed);
            self.emit(&key, state, HostChangeKind::Offline(reason), Utc::now());
        }
        was_online
    }

    /// 將 last_seen 超過 offline_after 的線上 agent 標為離線，
    /// 並移除離線超過 [`OFFLINE_RETENTION`] 且已無連線的 agent。
    pub fn sweep(&self, now: Instant) -> SweepResult {
        let mut result = SweepResult::default();
        let mut guard = self.lock();
        let inner = &mut *guard;
        let now_wall = Utc::now();
        // 只走訪仍有連線的 agent（線上的 agent 一定仍有連線）
        for agent_id in &inner.connected {
            let Some(state) = inner.agents.get_mut(agent_id) else {
                continue;
            };
            if !state.online {
                continue;
            }
            let silent_for = now.saturating_duration_since(state.last_seen);
            if silent_for > state.offline_after {
                state.online = false;
                state.offline_since.get_or_insert(now);
                state.offline_reason = Some(OfflineReason::Timeout);
                self.emit(
                    agent_id,
                    state,
                    HostChangeKind::Offline(OfflineReason::Timeout),
                    now_wall,
                );
                result.went_offline.push(WentOffline {
                    agent_id: agent_id.to_string(),
                    hostname: state.hostname.to_string(),
                    silent_for,
                });
            }
        }
        // 離線索引依時間排序：從最舊的開始，直到遇到未滿 24 小時者
        while let Some((&(since, _), _)) = inner.offline.first_key_value() {
            let offline_for = now.saturating_duration_since(since);
            if offline_for <= OFFLINE_RETENTION {
                break;
            }
            let Some((_, agent_id)) = inner.offline.pop_first() else {
                break;
            };
            if let Some(state) = inner.agents.remove(&agent_id) {
                result.removed.push(Removed {
                    agent_id: agent_id.to_string(),
                    hostname: state.hostname.to_string(),
                    offline_for,
                });
            }
        }
        drop(guard);
        result
            .went_offline
            .sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        result.removed.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        result
    }

    /// （線上數, 總數）
    pub fn counts(&self) -> (usize, usize) {
        let inner = self.lock();
        let online = inner
            .connected
            .iter()
            .filter(|id| inner.agents.get(*id).is_some_and(|s| s.online))
            .count();
        (online, inner.agents.len())
    }

    /// 產生 agents.json 用的快照（依 agent_id 排序）。
    /// 持鎖期間只複製參考計數與數值；字串複製、時間格式化、排序與序列化都在鎖外進行。
    pub fn snapshot(&self, now_wall: DateTime<Utc>) -> RegistrySnapshot {
        let raw: Vec<RawAgent> = {
            let inner = self.lock();
            inner
                .agents
                .iter()
                .map(|(agent_id, s)| RawAgent {
                    agent_id: agent_id.clone(),
                    hostname: s.hostname.clone(),
                    agent_version: s.agent_version.clone(),
                    proto: s.proto,
                    host: s.host.clone(),
                    peer: s.peer.clone(),
                    online: s.online,
                    conn_id: s.conn_id,
                    connected_at: s.connected_at,
                    last_seen: s.last_seen_wall,
                    offline_after: s.offline_after,
                    last_report: s.last_report.clone(),
                })
                .collect()
        };

        let mut list: Vec<AgentSnapshot> = raw
            .into_iter()
            .map(|r| AgentSnapshot {
                agent_id: r.agent_id.to_string(),
                hostname: r.hostname.to_string(),
                agent_version: r.agent_version.to_string(),
                proto: r.proto,
                host: r.host.map(|h| (*h).clone()),
                peer: r.peer.to_string(),
                online: r.online,
                conn_id: r.conn_id,
                connected_at: fmt_time(r.connected_at),
                last_seen: fmt_time(r.last_seen),
                offline_after_ms: u64::try_from(r.offline_after.as_millis()).unwrap_or(u64::MAX),
                last_report: r.last_report.map(|r| (*r).clone()),
            })
            .collect();
        list.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        RegistrySnapshot {
            generated_at: fmt_time(now_wall),
            online: list.iter().filter(|a| a.online).count(),
            total: list.len(),
            agents: list,
        }
    }

    /// 兩個索引與主表一致（測試用）
    #[cfg(test)]
    fn assert_consistent(&self) {
        let inner = self.lock();
        for (id, s) in &inner.agents {
            assert_eq!(s.connected, inner.connected.contains(id), "{id}");
            assert_eq!(s.connected, s.offline_key.is_none(), "{id}");
            if s.online {
                assert!(s.connected, "{id}");
            }
            if let Some(key) = s.offline_key {
                assert_eq!(inner.offline.get(&key), Some(id));
            }
        }
        assert!(
            inner
                .connected
                .iter()
                .all(|id| inner.agents.contains_key(id))
        );
        assert!(
            inner
                .offline
                .values()
                .all(|id| inner.agents.contains_key(id))
        );
        assert_eq!(
            inner.connected.len() + inner.offline.len(),
            inner.agents.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot::error::TryRecvError;

    fn hello(agent_id: &str, peer: &str) -> HelloInfo {
        HelloInfo {
            agent_id: agent_id.into(),
            hostname: format!("host-{agent_id}"),
            agent_version: "0.1.0".into(),
            proto: 1,
            host: None,
            peer: peer.into(),
            offline_after: Duration::from_millis(5000),
            client_chain: None,
        }
    }

    fn report(seq: u64) -> LastReport {
        LastReport {
            recv_at: "2026-10-08T00:00:00.000Z".into(),
            seq,
            ts: "2026-10-08T00:00:00.000Z".into(),
            cpu_usage: Some(1.5),
            total_memory: Some(100),
            used_memory: Some(50),
            metrics: None,
            detail: Default::default(),
        }
    }

    fn rejected(reason: &str) -> KickReason {
        KickReason::CertificateRejected(ChainRejection::new(reason, None))
    }

    fn ids_offline(r: &SweepResult) -> Vec<&str> {
        r.went_offline.iter().map(|g| g.agent_id.as_str()).collect()
    }

    fn ids_removed(r: &SweepResult) -> Vec<&str> {
        r.removed.iter().map(|g| g.agent_id.as_str()).collect()
    }

    #[test]
    fn conn_ids_are_unique_and_increasing() {
        let a = next_conn_id();
        let b = next_conn_id();
        assert!(b > a);
    }

    /// 收到的所有狀態變化：(agent_id, 種類)
    fn drain(rx: &mut broadcast::Receiver<HostChange>) -> Vec<(String, HostChangeKind)> {
        let mut out = Vec::new();
        while let Ok(c) = rx.try_recv() {
            out.push((c.agent_id.to_string(), c.kind));
        }
        out
    }

    #[test]
    fn state_changes_are_broadcast() {
        // kairos/API.md 第 6 節：上線、恢復、離線（含原因）各一則；沒有狀態變化時不送出
        let r = Registry::new();
        let mut rx = r.subscribe();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(drain(&mut rx), [("a".into(), HostChangeKind::Online)]);
        // 取代仍在線上的連線：沒有狀態變化
        let _a2 = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        assert!(drain(&mut rx).is_empty());
        assert_eq!(r.touch("a", 2, t0, Utc::now()), Touch::Updated);
        assert!(drain(&mut rx).is_empty());
        // sweep 判定離線 → 再收到訊息時恢復
        let later = t0 + Duration::from_secs(6);
        assert_eq!(ids_offline(&r.sweep(later)), ["a"]);
        assert_eq!(
            drain(&mut rx),
            [("a".into(), HostChangeKind::Offline(OfflineReason::Timeout))]
        );
        assert_eq!(
            r.live_get("a").unwrap().offline_reason,
            Some(OfflineReason::Timeout)
        );
        assert_eq!(r.touch("a", 2, later, Utc::now()), Touch::Revived);
        assert_eq!(drain(&mut rx), [("a".into(), HostChangeKind::Online)]);
        // 連線關閉
        assert!(r.disconnect("a", 2, later));
        assert_eq!(
            drain(&mut rx),
            [(
                "a".into(),
                HostChangeKind::Offline(OfflineReason::ConnectionClosed)
            )]
        );
        // 已離線的連線再結束：沒有事件
        assert!(!r.disconnect("a", 2, later));
        assert!(drain(&mut rx).is_empty());

        // 憑證被拒而被要求結束的連線：離線原因為 certificate_rejected
        let _b = r.register(3, hello("b", "p"), later, Utc::now()).unwrap();
        assert!(r.kick("b", 3, rejected("revoked")));
        assert!(r.disconnect("b", 3, later));
        let changes = drain(&mut rx);
        assert_eq!(
            changes,
            [
                ("b".into(), HostChangeKind::Online),
                (
                    "b".into(),
                    HostChangeKind::Offline(OfflineReason::CertificateRejected)
                )
            ]
        );
        // 關機
        let _c = r.register(4, hello("c", "p"), later, Utc::now()).unwrap();
        drain(&mut rx);
        assert_eq!(r.mark_all_offline(later), 1);
        assert_eq!(
            drain(&mut rx),
            [(
                "c".into(),
                HostChangeKind::Offline(OfflineReason::ServerShutdown)
            )]
        );
        r.assert_consistent();
    }

    #[test]
    fn live_view_reports_delivery_state() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let live = r.live_get("a").unwrap();
        assert!(live.online && live.seen_since_start);
        assert_eq!(live.durable, None);
        assert!(!live.replaying(t0));
        r.set_durable("a", 1, true);
        // conn_id 不符時不更新
        r.set_durable("a", 99, false);
        r.note_backfill("a", 1, 3_600_000, t0, Utc::now());
        let live = r.live_get("a").unwrap();
        assert_eq!(live.durable, Some(true));
        assert_eq!(live.backfill.map(|b| b.lag_ms), Some(3_600_000));
        assert!(live.replaying(t0 + Duration::from_secs(30)));
        assert!(!live.replaying(t0 + BACKFILL_ACTIVE + Duration::from_secs(1)));
        assert_eq!(r.live_view().len(), 1);
        assert_eq!(r.sizes(), (1, 1));
        assert!(r.live_get("missing").is_none());
    }

    #[test]
    fn first_register_is_new_and_online() {
        let r = Registry::new();
        let t0 = Instant::now();
        let reg = r
            .register(1, hello("a", "1.1.1.1:1"), t0, Utc::now())
            .unwrap();
        assert_eq!(reg.kind, RegisterKind::New);
        assert_eq!(r.counts(), (1, 1));
        r.assert_consistent();
    }

    #[test]
    fn second_connection_supersedes_first() {
        let r = Registry::new();
        let t0 = Instant::now();
        let mut first = r
            .register(1, hello("a", "1.1.1.1:1"), t0, Utc::now())
            .unwrap();
        assert_eq!(first.kick.try_recv(), Err(TryRecvError::Empty));

        let mut second = r
            .register(2, hello("a", "1.1.1.1:2"), t0, Utc::now())
            .unwrap();
        assert_eq!(
            second.kind,
            RegisterKind::Superseded {
                previous_conn_id: 1
            }
        );
        // 舊連線收到 kick
        assert_eq!(first.kick.try_recv(), Ok(KickReason::Superseded));
        // 新連線尚未被踢
        assert_eq!(second.kick.try_recv(), Err(TryRecvError::Empty));

        // 舊連線結束時 conn_id 不符，不得把 agent 標為離線
        assert!(!r.disconnect("a", 1, t0));
        assert_eq!(r.counts(), (1, 1));
        // 舊連線的 touch 也不影響狀態
        assert_eq!(r.touch("a", 1, t0, Utc::now()), Touch::Stale);
        assert_eq!(r.touch("a", 2, t0, Utc::now()), Touch::Updated);

        // 新連線結束才標為離線
        assert!(r.disconnect("a", 2, t0));
        assert_eq!(r.counts(), (0, 1));
        // 重複呼叫不再回報狀態變化
        assert!(!r.disconnect("a", 2, t0));
        // 連線已結束：touch 不再更新
        assert_eq!(r.touch("a", 2, t0, Utc::now()), Touch::Stale);
        r.assert_consistent();
    }

    #[test]
    fn reconnect_after_disconnect_is_reconnected_and_keeps_last_report() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _first = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(r.touch("a", 1, t0, Utc::now()), Touch::Updated);
        assert!(r.set_last_report("a", 1, report(7)));
        assert!(r.disconnect("a", 1, t0));

        let reg = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(reg.kind, RegisterKind::Reconnected);
        let snap = r.snapshot(Utc::now());
        assert_eq!(snap.agents[0].last_report.as_ref().map(|r| r.seq), Some(7));
        assert_eq!(snap.agents[0].conn_id, 2);
        assert!(snap.agents[0].online);
        r.assert_consistent();
    }

    #[test]
    fn durable_last_report_is_not_replaced_by_older_samples() {
        // A5：補送的舊資料不蓋過較新的 last_report
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let at = |seq, ts: &str| LastReport {
            ts: ts.into(),
            ..report(seq)
        };
        let last_seq = || {
            r.snapshot(Utc::now()).agents[0]
                .last_report
                .as_ref()
                .map(|r| r.seq)
        };
        assert!(r.set_last_report_unless_older("a", 1, at(1, "2026-10-08T03:00:00.000Z")));
        assert!(!r.set_last_report_unless_older("a", 1, at(2, "2026-10-08T02:59:59.999Z")));
        assert_eq!(last_seq(), Some(1));
        // 同一時間或較新的照常更新；任一方無法解析時也更新
        assert!(r.set_last_report_unless_older("a", 1, at(3, "2026-10-08T03:00:00.000Z")));
        assert!(r.set_last_report_unless_older("a", 1, at(4, "2026-10-08T11:00:00+08:00")));
        assert!(r.set_last_report_unless_older("a", 1, at(5, "not a time")));
        assert!(r.set_last_report_unless_older("a", 1, at(6, "2001-01-01T00:00:00Z")));
        assert_eq!(last_seq(), Some(6));
        // 連線不符時不更新
        assert!(!r.set_last_report_unless_older("a", 9, at(7, "2030-01-01T00:00:00Z")));
    }

    #[test]
    fn report_from_superseded_connection_is_stale() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        // 舊連線：touch 回傳 Stale（呼叫端據此不寫檔），也不能更新 last_report
        assert_eq!(r.touch("a", 1, t0, Utc::now()), Touch::Stale);
        assert!(!r.set_last_report("a", 1, report(1)));
        assert_eq!(r.snapshot(Utc::now()).agents[0].last_report, None);
        // 現役連線
        assert_eq!(r.touch("a", 2, t0, Utc::now()), Touch::Updated);
        assert!(r.set_last_report("a", 2, report(2)));
        assert_eq!(
            r.snapshot(Utc::now()).agents[0]
                .last_report
                .as_ref()
                .map(|r| r.seq),
            Some(2)
        );
        // 不存在的 agent 也是 Stale
        assert_eq!(r.touch("zzz", 2, t0, Utc::now()), Touch::Stale);
        assert!(!r.set_last_report("zzz", 2, report(3)));
    }

    #[test]
    fn kick_with_reason_and_live_connections() {
        let r = Registry::new();
        let t0 = Instant::now();
        let chain: ClientChain = Arc::from(vec![CertificateDer::from(vec![1u8, 2, 3])]);
        let mut info = hello("a", "p");
        info.client_chain = Some(chain.clone());
        let mut a = r.register(1, info, t0, Utc::now()).unwrap();
        let mut b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();

        let mut live = r.live_connections();
        live.sort_by(|x, y| x.agent_id.cmp(&y.agent_id));
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].conn_id, 1);
        assert_eq!(live[0].client_chain.as_deref(), Some(&chain[..]));
        assert!(live[1].client_chain.is_none());

        // conn_id 不符不踢
        assert!(!r.kick("a", 99, rejected("x")));
        assert_eq!(a.kick.try_recv(), Err(TryRecvError::Empty));
        // 踢掉 a
        assert!(r.kick("a", 1, rejected("revoked")));
        assert_eq!(a.kick.try_recv(), Ok(rejected("revoked")));
        // 已踢過不再出現在現役清單，也不能重複踢
        assert!(!r.kick("a", 1, KickReason::Superseded));
        assert_eq!(r.live_connections().len(), 1);
        assert_eq!(b.kick.try_recv(), Err(TryRecvError::Empty));
        // 連線結束後不在清單中
        r.disconnect("b", 2, t0);
        assert!(r.live_connections().is_empty());
        r.assert_consistent();
    }

    #[test]
    fn mark_all_offline_on_shutdown() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();
        assert!(r.disconnect("b", 2, t0));
        assert_eq!(r.mark_all_offline(t0), 1);
        assert_eq!(r.counts(), (0, 2));
        assert!(r.snapshot(Utc::now()).agents.iter().all(|a| !a.online));
        assert_eq!(r.mark_all_offline(t0), 0);
        r.assert_consistent();
    }

    #[test]
    fn sweep_marks_silent_agents_offline_once() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();

        // 剛好等於門檻不算離線
        assert_eq!(
            r.sweep(t0 + Duration::from_millis(5000)),
            SweepResult::default()
        );

        // b 在 t0+3s 有心跳
        r.touch("b", 2, t0 + Duration::from_secs(3), Utc::now());

        let res = r.sweep(t0 + Duration::from_millis(5001));
        assert_eq!(ids_offline(&res), ["a"]);
        assert_eq!(res.went_offline[0].silent_for, Duration::from_millis(5001));
        assert_eq!(r.counts(), (1, 2));

        // 已離線者不重複回報
        assert!(r.sweep(t0 + Duration::from_secs(6)).went_offline.is_empty());

        // b 之後也逾時
        let res = r.sweep(t0 + Duration::from_millis(8001));
        assert_eq!(ids_offline(&res), ["b"]);
        assert_eq!(r.counts(), (0, 2));
        r.assert_consistent();
    }

    #[test]
    fn touch_revives_swept_agent() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(
            r.touch("a", 1, t0 + Duration::from_secs(1), Utc::now()),
            Touch::Updated
        );
        assert_eq!(r.sweep(t0 + Duration::from_secs(7)).went_offline.len(), 1);
        assert_eq!(
            r.touch("a", 1, t0 + Duration::from_secs(8), Utc::now()),
            Touch::Revived
        );
        assert_eq!(r.counts(), (1, 1));
        r.assert_consistent();
    }

    #[test]
    fn sweep_respects_per_agent_offline_after() {
        let r = Registry::new();
        let t0 = Instant::now();
        let mut slow = hello("slow", "p");
        slow.offline_after = Duration::from_secs(15);
        let _s = r.register(1, slow, t0, Utc::now()).unwrap();
        let _f = r.register(2, hello("fast", "p"), t0, Utc::now()).unwrap();
        let res = r.sweep(t0 + Duration::from_secs(10));
        assert_eq!(ids_offline(&res), ["fast"]);
    }

    #[test]
    fn agents_offline_over_24h_are_removed() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();
        let _c = r.register(3, hello("c", "p"), t0, Utc::now()).unwrap();

        // a 在 t0 斷線
        assert!(r.disconnect("a", 1, t0));
        // b 在 t0+10s 由 sweep 判定離線，連線也隨後結束
        let res = r.sweep(t0 + Duration::from_secs(10));
        assert_eq!(ids_offline(&res), ["b", "c"]);
        r.disconnect("b", 2, t0 + Duration::from_secs(11));
        // c 被 sweep 判定離線，但連線仍在（kick 尚在），不可移除

        // 剛好 24 小時不移除
        assert!(r.sweep(t0 + OFFLINE_RETENTION).removed.is_empty());

        // a 超過 24 小時 → 移除；b 的離線時間從 t0+10s 起算，尚未超過
        let res = r.sweep(t0 + OFFLINE_RETENTION + Duration::from_secs(1));
        assert_eq!(ids_removed(&res), ["a"]);
        assert!(res.removed[0].offline_for > OFFLINE_RETENTION);
        assert_eq!(r.counts(), (0, 2));

        let res = r.sweep(t0 + OFFLINE_RETENTION + Duration::from_secs(11));
        assert_eq!(ids_removed(&res), ["b"]);
        // c 仍有連線，保留
        assert_eq!(r.counts(), (0, 1));
        assert_eq!(r.snapshot(Utc::now()).agents[0].agent_id, "c");
        r.assert_consistent();

        // 被移除的 agent 再出現時視為新 agent
        let reg = r.register(4, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(reg.kind, RegisterKind::New);
        r.assert_consistent();
    }

    #[test]
    fn revived_agent_resets_offline_clock() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert!(r.disconnect("a", 1, t0));
        // 重新連線後再斷線，離線時間要從新的斷線時間起算
        let t1 = t0 + Duration::from_secs(20 * 60 * 60);
        let _a2 = r.register(2, hello("a", "p"), t1, Utc::now()).unwrap();
        assert!(r.disconnect("a", 2, t1));
        assert!(
            r.sweep(t0 + OFFLINE_RETENTION + Duration::from_secs(1))
                .removed
                .is_empty()
        );
        assert_eq!(
            ids_removed(&r.sweep(t1 + OFFLINE_RETENTION + Duration::from_secs(1))),
            ["a"]
        );
        r.assert_consistent();
    }

    #[test]
    fn full_registry_evicts_oldest_offline_agent() {
        let r = Registry::with_max_agents(3);
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();
        let _c = r.register(3, hello("c", "p"), t0, Utc::now()).unwrap();
        // b 先離線、a 後離線，c 仍在線
        assert!(r.disconnect("b", 2, t0 + Duration::from_secs(1)));
        assert!(r.disconnect("a", 1, t0 + Duration::from_secs(2)));

        // 已存在的 agent 重新連線不需淘汰
        let again = r.register(4, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(again.kind, RegisterKind::Reconnected);
        assert_eq!(again.evicted, None);
        assert!(r.disconnect("a", 4, t0 + Duration::from_secs(3)));

        // 新的 agent：淘汰離線最久的 b
        let d = r
            .register(5, hello("d", "p"), t0 + Duration::from_secs(10), Utc::now())
            .unwrap();
        assert_eq!(d.kind, RegisterKind::New);
        let evicted = d.evicted.unwrap();
        assert_eq!(evicted.agent_id, "b");
        assert_eq!(evicted.offline_for, Duration::from_secs(9));
        assert_eq!(r.counts(), (2, 3));
        let ids: Vec<String> = r
            .snapshot(Utc::now())
            .agents
            .into_iter()
            .map(|a| a.agent_id)
            .collect();
        assert_eq!(ids, ["a", "c", "d"]);
        r.assert_consistent();
    }

    #[test]
    fn full_registry_rejects_new_agent_when_all_connected() {
        // K-S1：以隨機 agent_id 連線灌爆狀態表時，筆數不會超過上限
        let r = Registry::with_max_agents(2);
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();
        assert_eq!(
            r.register(3, hello("c", "p"), t0, Utc::now()).unwrap_err(),
            RegistryFull { max_agents: 2 }
        );
        assert_eq!(r.counts(), (2, 2));

        // 被 sweep 判定離線但連線仍在、或已被 kick 但尚未關閉的連線都不可淘汰
        r.sweep(t0 + Duration::from_secs(10));
        assert!(r.kick("a", 1, rejected("x")));
        assert!(r.register(3, hello("c", "p"), t0, Utc::now()).is_err());

        // 既有 agent 的取代不受上限影響
        let b2 = r.register(4, hello("b", "p"), t0, Utc::now()).unwrap();
        assert_eq!(
            b2.kind,
            RegisterKind::Superseded {
                previous_conn_id: 2
            }
        );

        // 連線結束後才可淘汰
        r.disconnect("a", 1, t0 + Duration::from_secs(11));
        let c = r.register(5, hello("c", "p"), t0, Utc::now()).unwrap();
        assert_eq!(c.evicted.map(|e| e.agent_id), Some("a".to_string()));

        // 大量隨機 agent_id 輪流連線、斷線：筆數始終不超過上限
        for i in 0..1000u64 {
            let id = format!("flood-{i}");
            if r.register(100 + i, hello(&id, "p"), t0, Utc::now()).is_ok() {
                r.disconnect(&id, 100 + i, t0);
            }
            assert!(r.counts().1 <= 2);
        }
        r.assert_consistent();
    }

    #[test]
    fn eviction_uses_the_offline_index_in_time_order() {
        // R2-K15：表滿時依離線時間淘汰，不必掃描整張表；索引與主表保持一致
        let r = Registry::with_max_agents(100);
        let t0 = Instant::now();
        for i in 0..100u64 {
            let id = format!("a{i:03}");
            let _ = r.register(i, hello(&id, "p"), t0, Utc::now()).unwrap();
            // 反向的離線時間：a099 最早離線
            assert!(r.disconnect(&id, i, t0 + Duration::from_secs(1000 - i)));
        }
        for (n, expected) in ["a099", "a098", "a097"].into_iter().enumerate() {
            let id = format!("new{n}");
            let reg = r
                .register(1000 + n as u64, hello(&id, "p"), t0, Utc::now())
                .unwrap();
            assert_eq!(reg.evicted.unwrap().agent_id, expected);
        }
        assert_eq!(r.counts(), (3, 100));
        r.assert_consistent();
    }

    #[test]
    fn reconnect_while_rejected_connection_is_closing_is_superseded() {
        // K-C8：舊連線已收到 certificate_rejected 但尚未關閉時，新連線應記為「取代」
        let r = Registry::new();
        let t0 = Instant::now();
        let _old = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert!(r.kick("a", 1, rejected("revoked")));
        let new = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(
            new.kind,
            RegisterKind::Superseded {
                previous_conn_id: 1
            }
        );
        // 舊連線之後才結束：conn_id 不符，不影響新連線
        assert!(!r.disconnect("a", 1, t0));
        assert_eq!(r.counts(), (1, 1));
        r.assert_consistent();
    }

    #[test]
    fn client_chain_is_dropped_on_disconnect() {
        // K-S13：斷線後不再保留客戶端憑證鏈
        let r = Registry::new();
        let t0 = Instant::now();
        let chain: ClientChain = Arc::from(vec![CertificateDer::from(vec![1u8, 2, 3])]);
        let mut info = hello("a", "p");
        info.client_chain = Some(chain.clone());
        let _a = r.register(1, info, t0, Utc::now()).unwrap();
        let mut info_b = hello("b", "p");
        info_b.client_chain = Some(chain.clone());
        let _b = r.register(2, info_b, t0, Utc::now()).unwrap();
        assert_eq!(Arc::strong_count(&chain), 3);

        assert!(r.disconnect("a", 1, t0));
        assert!(r.lock().agents.get("a").unwrap().client_chain.is_none());
        r.mark_all_offline(t0);
        assert!(r.lock().agents.get("b").unwrap().client_chain.is_none());
        assert_eq!(Arc::strong_count(&chain), 1);
    }

    fn hello_v2(agent_id: &str, fingerprint: Option<&str>) -> HelloInfo {
        HelloInfo {
            proto: 2,
            host: Some(Arc::new(HostInfo {
                host_fingerprint: fingerprint.map(str::to_string),
                display_name: Some("Web 01".into()),
                tags: Some(vec!["env:prod".into()]),
                ..Default::default()
            })),
            ..hello(agent_id, "p")
        }
    }

    #[test]
    fn changed_host_fingerprint_is_reported() {
        let r = Registry::new();
        let t0 = Instant::now();
        let fp_a = format!("sha256:{}", "a".repeat(64));
        let fp_b = format!("sha256:{}", "b".repeat(64));
        let first = r
            .register(1, hello_v2("x", Some(&fp_a)), t0, Utc::now())
            .unwrap();
        assert_eq!(first.previous_fingerprint, None);
        // 相同指紋、沒有指紋（v1 或未送）都不算改變
        let same = r
            .register(2, hello_v2("x", Some(&fp_a)), t0, Utc::now())
            .unwrap();
        assert_eq!(same.previous_fingerprint, None);
        let v1 = r.register(3, hello("x", "p"), t0, Utc::now()).unwrap();
        assert_eq!(v1.previous_fingerprint, None);
        let _ = r
            .register(4, hello_v2("x", Some(&fp_a)), t0, Utc::now())
            .unwrap();
        let changed = r
            .register(5, hello_v2("x", Some(&fp_b)), t0, Utc::now())
            .unwrap();
        assert_eq!(changed.previous_fingerprint, Some(fp_a));
        assert!(matches!(changed.kind, RegisterKind::Superseded { .. }));
    }

    #[test]
    fn snapshot_includes_proto_and_host_info() {
        let r = Registry::new();
        let t0 = Instant::now();
        let fp = format!("sha256:{}", "a".repeat(64));
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r
            .register(2, hello_v2("b", Some(&fp)), t0, Utc::now())
            .unwrap();
        let snap = serde_json::to_value(r.snapshot(Utc::now())).unwrap();
        let a = &snap["agents"][0];
        let b = &snap["agents"][1];
        // v1 agent：只多了 proto，沒有任何主機資訊欄位
        assert_eq!(a["proto"], 1);
        for field in [
            "host_fingerprint",
            "display_name",
            "tags",
            "report_interval_ms",
            "os",
        ] {
            assert!(a.get(field).is_none(), "{field}: {a}");
        }
        assert_eq!(b["proto"], 2);
        assert_eq!(b["host_fingerprint"], fp.as_str());
        assert_eq!(b["display_name"], "Web 01");
        assert_eq!(b["tags"], serde_json::json!(["env:prod"]));
        assert!(b.get("os").is_none(), "{b}");
    }

    fn restored(
        agent_id: &str,
        offline_for: chrono::Duration,
        now: DateTime<Utc>,
    ) -> RestoredAgent {
        RestoredAgent {
            agent_id: agent_id.into(),
            hostname: format!("host-{agent_id}"),
            agent_version: "0.2.0".into(),
            proto: 2,
            host: Some(HostInfo {
                host_fingerprint: Some(format!("sha256:{}", "a".repeat(64))),
                ..Default::default()
            }),
            peer: "10.0.0.1:5000".into(),
            connected_at: now - offline_for - chrono::Duration::hours(1),
            last_seen: now - offline_for,
            offline_after: Duration::from_secs(75),
            last_report: Some(report(3)),
        }
    }

    #[test]
    fn restore_puts_recent_hosts_back_as_offline() {
        // A6：重新啟動後，清冊中 24 小時內出現過的 agent 以離線狀態回到狀態表
        let r = Registry::with_max_agents(3);
        let t0 = Instant::now();
        let now = Utc::now();
        let n = r.restore(
            vec![
                restored("old", chrono::Duration::hours(25), now),
                restored("a", chrono::Duration::hours(2), now),
                restored("b", chrono::Duration::minutes(1), now),
            ],
            t0,
            now,
        );
        assert_eq!(n, 2);
        assert_eq!(r.counts(), (0, 2));
        r.assert_consistent();
        let snap = r.snapshot(now);
        let a = &snap.agents[0];
        assert_eq!(a.agent_id, "a");
        assert!(!a.online);
        assert_eq!(a.proto, 2);
        assert_eq!(a.last_report.as_ref().map(|l| l.seq), Some(3));
        assert_eq!(a.last_seen, fmt_time(now - chrono::Duration::hours(2)));
        assert_eq!(a.offline_after_ms, 75_000);

        // 重新連線：記為「重新上線」，並延續主機指紋的比對
        let mut info = hello("a", "p");
        info.host = Some(Arc::new(HostInfo {
            host_fingerprint: Some(format!("sha256:{}", "b".repeat(64))),
            ..Default::default()
        }));
        let reg = r.register(1, info, t0, now).unwrap();
        assert_eq!(reg.kind, RegisterKind::Reconnected);
        assert_eq!(
            reg.previous_fingerprint,
            Some(format!("sha256:{}", "a".repeat(64)))
        );
        r.assert_consistent();
        // 舊的離線時間照常計算：b 離線 1 分鐘，24 小時後被移除
        let res = r.sweep(t0 + OFFLINE_RETENTION);
        assert_eq!(ids_removed(&res), ["b"]);
        r.assert_consistent();

        // 已在表中的 agent 不覆寫；超過上限的不放回
        let r = Registry::with_max_agents(1);
        assert_eq!(
            r.restore(
                vec![
                    restored("x", chrono::Duration::hours(1), now),
                    restored("y", chrono::Duration::hours(2), now)
                ],
                t0,
                now
            ),
            1
        );
        assert_eq!(r.snapshot(now).agents[0].agent_id, "x");
        assert_eq!(
            r.restore(vec![restored("x", chrono::Duration::zero(), now)], t0, now),
            0
        );
    }

    #[test]
    fn snapshot_is_sorted_and_counts() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _b = r.register(1, hello("b", "p"), t0, Utc::now()).unwrap();
        let _a = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        r.disconnect("b", 1, t0);
        let snap = r.snapshot(Utc::now());
        assert_eq!(snap.total, 2);
        assert_eq!(snap.online, 1);
        assert_eq!(snap.agents[0].agent_id, "a");
        assert_eq!(snap.agents[1].agent_id, "b");
        assert!(!snap.agents[1].online);
        assert_eq!(snap.agents[0].offline_after_ms, 5000);
        assert_eq!(snap.agents[0].hostname, "host-a");
    }

    #[test]
    fn counter_rates_and_reboots_are_derived_from_the_previous_report() {
        let r = Registry::new();
        let bus = r.bus();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        // 上線事件之後才訂閱
        let mut events = bus.subscribe();
        let at = |seq, ts: &str, rx: u64, uptime: u64| LastReport {
            ts: ts.into(),
            detail: ReportDetail {
                uptime_secs: Some(uptime),
                network: vec![IoLast {
                    device: "eth0".into(),
                    rx: Some(rx),
                    tx: Some(0),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..report(seq)
        };
        let rx_rate = || r.live_get("a").unwrap().last_report.unwrap().detail.network[0].rx_rate;
        assert!(r.set_last_report("a", 1, at(1, "2026-10-08T00:00:00Z", 1000, 100)));
        assert_eq!(rx_rate(), None);
        assert!(r.set_last_report("a", 1, at(2, "2026-10-08T00:00:05Z", 6000, 105)));
        assert_eq!(rx_rate(), Some(1000.0));
        assert!(events.try_recv().is_err(), "沒有重新開機");
        // uptime 變小：重新開機（沒有速率），發布事件
        assert!(r.set_last_report_unless_older("a", 1, at(3, "2026-10-08T00:00:10Z", 7000, 3)));
        assert_eq!(rx_rate(), None);
        let ev = events.try_recv().unwrap();
        match &ev.kind {
            EventKind::HostRebooted {
                agent_id,
                uptime_secs,
                previous_uptime_secs,
                boot_time,
                ..
            } => {
                assert_eq!(agent_id, "a");
                assert_eq!((*uptime_secs, *previous_uptime_secs), (3, 105));
                assert_eq!(boot_time.as_deref(), Some("2026-10-08T00:00:07.000Z"));
            }
            other => panic!("{other:?}"),
        }
        // 補送的舊資料不取代、也不判斷重新開機
        assert!(!r.set_last_report_unless_older("a", 1, at(4, "2026-10-08T00:00:01Z", 1, 1)));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn report_detail_is_kept_in_the_agents_snapshot_format() {
        let d = ReportDetail {
            uptime_secs: Some(5),
            filesystems: vec![FsLast {
                mountpoint: "/".into(),
                usage: Some(75.0),
                estimated: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let lr = LastReport {
            detail: d,
            ..report(1)
        };
        let v = serde_json::to_value(&lr).unwrap();
        assert_eq!(v["uptime_secs"], 5);
        assert_eq!(v["filesystems"][0]["estimated"], true);
        // 沒有 F4 指標時與 0.7 的格式相同
        let old = serde_json::to_value(report(1)).unwrap();
        assert!(old.get("filesystems").is_none() && old.get("uptime_secs").is_none());
        let back: LastReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, lr);
        assert_eq!(back.detail.top_filesystem().unwrap().mountpoint, "/");
    }
}
