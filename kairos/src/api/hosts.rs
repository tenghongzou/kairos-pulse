//! 主機清冊：列表（篩選、排序、游標分頁、總數）與單台詳情（`kairos/API.md` 第 3、5.1、5.2 節；
//! 設計包 §24 #1、#4）。
//!
//! 列表由記憶體中的索引（[`Snapshot`]）回答：主機清冊（SQLite `hosts`，最多 [`DB_CACHE_MAX_AGE`]
//! 讀一次）與狀態表的即時狀態（`Registry::live_view`）合併，最多 [`SNAPSHOT_MAX_AGE`] 重建一次。
//! 1 萬台時重建約數毫秒、篩選與排序約 1～2 毫秒（第 9 節），不在每個請求都查詢資料庫，也不會長時間
//! 持有狀態表的鎖。單台詳情則每次讀取最新的清冊列與狀態。

use crate::store::prelude::*;
use chrono::{DateTime, Utc};
use hyper::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ApiContext;
use super::db::query_failed;
use super::http::{ApiBody, ApiError, Query, b64url_decode, b64url_encode, ok, ts, ts_ms};
use crate::hosts_cli::parse_duration;
use crate::per_ip::cidr::Cidr;
use crate::protocol::HostInfo;
use crate::rbac::HostAccess;
use crate::registry::{LastReport, LiveAgent};
use crate::tls::CertRejection;

/// 狀態表與清冊合併的索引最多這麼舊
pub const SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(1);
/// 清冊（資料庫）最多這麼久讀一次（清冊本身每 30 秒才從狀態表同步一次）
pub const DB_CACHE_MAX_AGE: Duration = Duration::from_secs(15);
/// Kairos 啟動後的暖機期：這段期間內、啟動前已離線且尚未連上的主機為 `unknown`
pub const STARTUP_GRACE: Duration = Duration::from_secs(120);
/// 「長期離線」的門檻（設計包 `ia.md` §5.2，預設 24 小時）
pub const LONG_OFFLINE_SECS: i64 = 24 * 60 * 60;
/// 列表每頁的預設與上限
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;

/// 主機狀態（設計包 `ia.md` §5.1，依嚴重度排序）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Critical,
    Offline,
    Warning,
    Unknown,
    Pending,
    Online,
}

impl Status {
    pub const ALL: [Status; 6] = [
        Status::Critical,
        Status::Offline,
        Status::Warning,
        Status::Unknown,
        Status::Pending,
        Status::Online,
    ];

    /// 本版能判定的狀態（其餘需要告警或註冊流程）
    pub fn supported(self) -> bool {
        matches!(self, Status::Offline | Status::Unknown | Status::Online)
    }

    pub fn rank(self) -> i64 {
        Status::ALL.iter().position(|s| *s == self).unwrap_or(0) as i64
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Critical => "critical",
            Status::Offline => "offline",
            Status::Warning => "warning",
            Status::Unknown => "unknown",
            Status::Pending => "pending",
            Status::Online => "online",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Status::ALL.into_iter().find(|x| x.as_str() == s)
    }
}

/// 各狀態的台數（不支援的狀態為 `null`）
pub fn status_counts(counts: &HashMap<Status, u64>) -> Value {
    let mut map = serde_json::Map::new();
    for s in Status::ALL {
        let v = if s.supported() {
            json!(counts.get(&s).copied().unwrap_or(0))
        } else {
            Value::Null
        };
        map.insert(s.as_str().to_string(), v);
    }
    Value::Object(map)
}

/// 清冊中的一台主機（`hosts` 的一列）
#[derive(Debug, Clone)]
pub struct DbHost {
    pub agent_id: Arc<str>,
    pub hostname: Arc<str>,
    pub display_name: Option<Arc<str>>,
    pub agent_version: Option<Arc<str>>,
    pub proto: Option<i64>,
    pub host_fingerprint: Option<Arc<str>>,
    pub tags: Arc<[String]>,
    pub os: Option<Arc<kairos_proto::OsInfo>>,
    pub report_interval_ms: Option<i64>,
    pub peer: Option<Arc<str>>,
    pub identity_verified: bool,
    pub first_seen: i64,
    pub connected_at: Option<i64>,
    pub last_seen: i64,
    pub offline_after_ms: Option<i64>,
    pub last_report: Option<Arc<LastReport>>,
    pub imported: bool,
    pub rejection: Option<CertRejection>,
    /// 主機群組（角色指派的範圍；`hosts.host_group`）
    pub host_group: Option<Arc<str>>,
    /// 開機時間（Unix 秒；`hosts.boot_time`，資產更新）
    pub boot_time: Option<i64>,
    /// 資產摘要（`host_inventory`；沒有資產時 `None`）
    pub inventory: Option<Arc<InventorySummary>>,
    /// 最近一次因每主機序列數上限而有指標未保存的時間（Unix 毫秒；`KRS-W4104`）
    pub series_limited_at: Option<i64>,
    /// hello 的 `install`（清冊同步；0.9.0 起的 Pulse）
    pub install: Option<Arc<kairos_proto::InstallInfo>>,
    /// 集中設定的狀態（`host_config`；沒有這台的紀錄時 `None`）
    pub config: Option<Arc<crate::remote::desired::ConfigStateInput>>,
}

/// 主機清單（S04）篩選與排序用的資產摘要（`host_inventory` 的冗餘欄位；METRICS.md 第 10.1 節）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InventorySummary {
    pub os_family: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_logical_cores: Option<i64>,
    pub memory_total: Option<i64>,
    pub vendor: Option<String>,
    pub product: Option<String>,
    /// 只在詳情輸出（清單不輸出，減少大量匯出時的外流面）
    pub serial: Option<String>,
}

impl InventorySummary {
    /// 清單的 `inventory`（不含序號）；`with_serial` 時另加序號（詳情）
    pub fn json(&self, with_serial: bool) -> Value {
        let mut v = json!({
            "os_family": self.os_family,
            "cpu_model": self.cpu_model,
            "cpu_logical_cores": self.cpu_logical_cores,
            "memory_total": self.memory_total,
            "vendor": self.vendor,
            "product": self.product,
        });
        if with_serial {
            v["serial"] = json!(self.serial);
        }
        v
    }
}

/// 清冊中保存的被拒憑證（`hosts.cert_rejection`）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRejection {
    pub reason: String,
    pub serial: Option<String>,
    pub peer: Option<String>,
    pub count: u64,
}

/// 列表與詳情共用的一台主機
#[derive(Debug, Clone)]
pub struct HostView {
    pub agent_id: Arc<str>,
    pub hostname: Arc<str>,
    pub display_name: Option<Arc<str>>,
    pub status: Status,
    pub never_connected: bool,
    pub starting: bool,
    pub offline_reason: Option<&'static str>,
    pub ip: Option<IpAddr>,
    pub peer: Option<Arc<str>>,
    pub agent_version: Option<Arc<str>>,
    pub proto: Option<i64>,
    pub os: Option<Arc<kairos_proto::OsInfo>>,
    pub tags: Arc<[String]>,
    pub host_fingerprint: Option<Arc<str>>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub connected_at: Option<i64>,
    pub offline_after_ms: Option<i64>,
    pub report_interval_ms: Option<i64>,
    pub identity_verified: bool,
    pub durable: Option<bool>,
    pub replaying: bool,
    pub last_report: Option<Arc<LastReport>>,
    pub imported: bool,
    pub rejection: Option<CertRejection>,
    /// 主機群組（角色指派的範圍；`hosts.host_group`）
    pub host_group: Option<Arc<str>>,
    /// 開機時間（Unix 秒）
    pub boot_time: Option<i64>,
    /// 資產摘要
    pub inventory: Option<Arc<InventorySummary>>,
    /// hello 的 `install`（連線中以這次的 hello 為準）
    pub install: Option<Arc<kairos_proto::InstallInfo>>,
    /// 集中設定的狀態（[`crate::remote::desired::config_state`]）
    pub config_state: &'static str,
    /// 排序與搜尋用：顯示名稱（沒有時為主機名稱）的小寫
    name_lower: String,
    hostname_lower: String,
    cpu: Option<f64>,
    mem_ratio: Option<f64>,
}

impl HostView {
    /// 不是 `online` 時離線多久（秒）
    pub fn offline_for_secs(&self, now_ms: i64) -> Option<i64> {
        (self.status != Status::Online).then(|| ((now_ms - self.last_seen) / 1000).max(0))
    }

    pub fn cpu(&self) -> Option<f64> {
        self.cpu
    }

    pub fn mem_ratio(&self) -> Option<f64> {
        self.mem_ratio
    }

    /// 使用率最高的掛載點：(掛載點, 使用率 %, 是否為推算值)
    pub fn top_disk(&self) -> Option<(&str, f64, bool)> {
        let fs = self.last_report.as_deref()?.detail.top_filesystem()?;
        Some((fs.mountpoint.as_str(), fs.usage?, fs.estimated))
    }

    /// 開機後經過的秒數：最後一筆 report 的 `system.uptime`（加上之後經過的時間），或由開機時間算出
    pub fn uptime_secs(&self, now_ms: i64) -> Option<i64> {
        if self.status != Status::Online {
            return None;
        }
        let from_report = self.last_report.as_deref().and_then(|r| {
            let up = i64::try_from(r.detail.uptime_secs?).ok()?;
            let at = DateTime::parse_from_rfc3339(&r.ts)
                .ok()
                .map(|t| t.timestamp_millis())?;
            Some(up + ((now_ms - at) / 1000).max(0))
        });
        from_report.or_else(|| self.boot_time.map(|b| (now_ms / 1000 - b).max(0)))
    }

    /// 開機時間的估計（Unix 秒）：最後一筆 report 的 `ts − system.uptime`，或資產的開機時間
    pub fn boot_estimate(&self) -> Option<i64> {
        let from_report = self.last_report.as_deref().and_then(|r| {
            let up = i64::try_from(r.detail.uptime_secs?).ok()?;
            let at = DateTime::parse_from_rfc3339(&r.ts).ok()?.timestamp();
            Some(at - up)
        });
        from_report.or(self.boot_time)
    }

    /// 作業系統家族：資產優先，沒有時由 hello 的 OS 名稱推斷
    pub fn os_family(&self) -> Option<String> {
        if let Some(f) = self.inventory.as_ref().and_then(|i| i.os_family.clone()) {
            return Some(f);
        }
        let name = self.os.as_ref()?.name.as_deref()?.to_lowercase();
        Some(if name.contains("windows") {
            "windows".to_string()
        } else {
            "linux".to_string()
        })
    }

    /// 列表中的一台主機（`kairos/API.md` 第 5.1 節）
    pub fn summary(&self, now_ms: i64) -> Value {
        let offline_for = self.offline_for_secs(now_ms);
        json!({
            "agent_id": &*self.agent_id,
            "hostname": &*self.hostname,
            // 主機名稱為假名（privacy.hostname = "pseudonymize"；PRIVACY.md 第 3 節）
            "pseudonymized": crate::privacy::is_host_pseudonym(&self.hostname),
            "display_name": self.display_name.as_deref(),
            "status": self.status.as_str(),
            "status_detail": {
                "offline_reason": self.offline_reason,
                // 0.7 過渡期：0.4～0.6 的名稱（timeout、connection_closed），0.8 移除（API.md 第 5.2 節）
                "offline_reason_legacy": self.offline_reason.map(crate::registry::legacy_offline_reason),
                "never_connected": self.never_connected,
                "starting": self.starting,
                "long_offline": offline_for.is_some_and(|s| s > LONG_OFFLINE_SECS),
            },
            "ip": self.ip.map(|ip| ip.to_string()),
            "agent_version": self.agent_version.as_deref(),
            "proto": self.proto,
            "os": self.os.as_deref().map(os_json),
            "tags": &*self.tags,
            "group": self.host_group.as_deref(),
            // 遠端管理（0.9.0；API.md 第 17 節）：安裝方式與本機狀態、集中設定的狀態
            "install": self.install.as_deref().map(|i| json!({"method": i.method, "upgrade": i.upgrade, "config": i.config})),
            "config_state": self.config_state,
            "first_seen": ts_ms(self.first_seen),
            "last_seen": ts_ms(self.last_seen),
            "offline_for_secs": offline_for,
            "offline_after_ms": self.offline_after_ms,
            "report_interval_ms": self.report_interval_ms,
            "identity_verified": self.identity_verified,
            "durable": self.durable,
            "replaying": self.durable.map(|_| self.replaying),
            "last_report": self.last_report.as_deref().map(report_summary),
            "source": if self.imported { "import" } else { "agent" },
            "os_family": self.os_family(),
            "inventory": self.inventory.as_deref().map(|i| i.json(false)),
            "boot_time": self.boot_time.and_then(|b| DateTime::from_timestamp(b, 0)).map(ts),
            "uptime_secs": self.uptime_secs(now_ms),
        })
    }
}

fn os_json(os: &kairos_proto::OsInfo) -> Value {
    json!({ "name": os.name, "version": os.version, "kernel": os.kernel, "arch": os.arch })
}

const BYTES_PER_MB: u64 = 1024 * 1024;

/// 最近一次 report 的摘要（記憶體以 bytes 表示；LastReport 以 MB 保存）
fn report_summary(r: &LastReport) -> Value {
    json!({
        "ts": r.ts,
        "cpu_usage": r.cpu_usage,
        "memory_used_bytes": r.used_memory.map(|m| m.saturating_mul(BYTES_PER_MB)),
        "memory_total_bytes": r.total_memory.map(|m| m.saturating_mul(BYTES_PER_MB)),
        "disk": r.detail.top_filesystem().map(|f| json!({
            "mountpoint": f.mountpoint,
            "usage": f.usage,
            "estimated": f.estimated,
        })),
    })
}

/// peer（`ip:port`）中的 IP
pub fn ip_of(peer: &str) -> Option<IpAddr> {
    crate::bus::peer_ip(peer)
}

/// 合併時需要的環境
#[derive(Debug, Clone, Copy)]
pub struct MergeEnv {
    pub now: DateTime<Utc>,
    pub now_i: Instant,
    /// Kairos 啟動的時間
    pub started: DateTime<Utc>,
    pub uptime: Duration,
    /// 是否啟用 mTLS（尚未寫入清冊的新主機）
    pub mtls: bool,
}

/// 清冊與即時狀態合併成一台主機
pub fn view_of(
    db: Option<&DbHost>,
    live: Option<&LiveAgent>,
    rejection: Option<&CertRejection>,
    env: &MergeEnv,
) -> Option<HostView> {
    let agent_id = live
        .map(|l| l.agent_id.clone())
        .or(db.map(|d| d.agent_id.clone()))?;
    let host: Option<&HostInfo> = live.and_then(|l| l.host.as_deref());
    let seen = live.is_some_and(|l| l.seen_since_start);
    let hostname = live
        .map(|l| l.hostname.clone())
        .or(db.map(|d| d.hostname.clone()))?;
    let display_name = host
        .and_then(|h| h.display_name.as_deref())
        .map(Arc::<str>::from)
        .or(db.and_then(|d| d.display_name.clone()));
    let peer = live
        .map(|l| l.peer.clone())
        .filter(|p| !p.is_empty())
        .or(db.and_then(|d| d.peer.clone()));
    let last_seen = live.map_or_else(
        || db.map_or(0, |d| d.last_seen),
        |l| l.last_seen.timestamp_millis(),
    );
    let last_report = live
        .and_then(|l| l.last_report.clone())
        .or(db.and_then(|d| d.last_report.clone()));
    // 記憶體中的是這次啟動後的最新一次；次數取兩者較大的（清冊中的含之前的累計）
    let rejection = match (rejection.cloned(), db.and_then(|d| d.rejection.clone())) {
        (Some(mut m), Some(d)) => {
            m.count = m.count.max(d.count);
            Some(m)
        }
        (m, d) => m.or(d),
    };
    let imported = db.is_some_and(|d| d.imported);
    let started_ms = env.started.timestamp_millis();

    let (status, never_connected, starting) = match live {
        Some(l) if l.online => (Status::Online, false, false),
        _ if seen => (Status::Offline, false, false),
        _ if db.is_some_and(|d| d.connected_at.is_none() && d.imported) => {
            (Status::Unknown, true, false)
        }
        _ if env.uptime < STARTUP_GRACE && last_seen < started_ms => (Status::Unknown, false, true),
        _ => (Status::Offline, false, false),
    };
    let offline_reason = if status == Status::Online {
        None
    } else if rejection
        .as_ref()
        .is_some_and(|r| r.at.timestamp_millis() > last_seen)
    {
        Some("certificate_rejected")
    } else if seen {
        live.and_then(|l| l.offline_reason).map(|r| r.as_str())
    } else {
        None
    };
    let cpu = last_report
        .as_ref()
        .and_then(|r| r.cpu_usage)
        .map(f64::from);
    let mem_ratio = last_report
        .as_ref()
        .and_then(|r| match (r.used_memory, r.total_memory) {
            (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64),
            _ => None,
        });
    let name = display_name.as_deref().unwrap_or(&hostname).to_lowercase();
    let install = host
        .and_then(|h| h.install.clone())
        .map(Arc::new)
        .or(db.and_then(|d| d.install.clone()));
    let version: Option<Arc<str>> = live
        .map(|l| l.agent_version.clone())
        .filter(|v| !v.is_empty())
        .or(db.and_then(|d| d.agent_version.clone()));
    let config_state = crate::remote::desired::config_state(
        version.as_deref(),
        install.as_ref().and_then(|i| i.config.as_deref()),
        db.and_then(|d| d.config.as_deref()),
    );
    Some(HostView {
        hostname_lower: hostname.to_lowercase(),
        name_lower: name,
        agent_id,
        hostname,
        display_name,
        status,
        never_connected,
        starting,
        offline_reason,
        ip: peer.as_deref().and_then(ip_of),
        peer,
        agent_version: version,
        proto: live
            .map(|l| i64::from(l.proto))
            .or(db.and_then(|d| d.proto)),
        os: host
            .and_then(|h| h.os.clone())
            .map(Arc::new)
            .or(db.and_then(|d| d.os.clone())),
        tags: host
            .and_then(|h| h.tags.clone())
            .map(Arc::<[String]>::from)
            .or(db.map(|d| d.tags.clone()))
            .unwrap_or_else(|| Arc::from(Vec::new())),
        host_fingerprint: host
            .and_then(|h| h.host_fingerprint.as_deref())
            .map(Arc::<str>::from)
            .or(db.and_then(|d| d.host_fingerprint.clone())),
        first_seen: db.map_or_else(
            || live.map_or(0, |l| l.connected_at.timestamp_millis()),
            |d| d.first_seen,
        ),
        last_seen,
        connected_at: if seen {
            live.map(|l| l.connected_at.timestamp_millis())
        } else {
            db.and_then(|d| d.connected_at)
        },
        offline_after_ms: live
            .map(|l| i64::try_from(l.offline_after.as_millis()).unwrap_or(i64::MAX))
            .or(db.and_then(|d| d.offline_after_ms)),
        report_interval_ms: host
            .and_then(|h| h.report_interval_ms)
            .and_then(|v| i64::try_from(v).ok())
            .or(db.and_then(|d| d.report_interval_ms)),
        identity_verified: db.map_or(env.mtls, |d| d.identity_verified || (seen && env.mtls)),
        durable: if seen {
            live.and_then(|l| l.durable)
        } else {
            None
        },
        replaying: live.is_some_and(|l| l.replaying(env.now_i)),
        last_report,
        imported,
        rejection,
        host_group: db.and_then(|d| d.host_group.clone()),
        boot_time: db.and_then(|d| d.boot_time),
        inventory: db.and_then(|d| d.inventory.clone()),
        install,
        config_state,
        cpu,
        mem_ratio,
    })
}

/// 合併後的索引
#[derive(Debug)]
pub struct Snapshot {
    pub at: DateTime<Utc>,
    pub hosts: Vec<HostView>,
    /// 建立索引之前最新的事件編號（之後的事件都可能讓索引過時）
    pub last_event_id: String,
}

/// 建立索引：清冊中的主機（以即時狀態覆蓋）＋尚未寫入清冊的新主機
pub fn build_snapshot(
    db: &[DbHost],
    live: Vec<LiveAgent>,
    rejections: &HashMap<String, CertRejection>,
    env: &MergeEnv,
    last_event_id: String,
) -> Snapshot {
    let mut live: HashMap<Arc<str>, LiveAgent> =
        live.into_iter().map(|l| (l.agent_id.clone(), l)).collect();
    let mut hosts = Vec::with_capacity(db.len() + live.len());
    for d in db {
        let l = live.remove(&d.agent_id);
        if let Some(v) = view_of(Some(d), l.as_ref(), rejections.get(&*d.agent_id), env) {
            hosts.push(v);
        }
    }
    for l in live.values() {
        if let Some(v) = view_of(None, Some(l), rejections.get(&*l.agent_id), env) {
            hosts.push(v);
        }
    }
    Snapshot {
        at: env.now,
        hosts,
        last_event_id,
    }
}

/// 清冊的快取：(讀取的時間, 當時最大的 host_id, 主機)
type DbCache = (Instant, i64, Arc<Vec<DbHost>>);

/// 索引的快取
#[derive(Default)]
pub struct HostIndex {
    db: tokio::sync::Mutex<Option<DbCache>>,
    snapshot: tokio::sync::Mutex<Option<(Instant, Arc<Snapshot>)>>,
    /// 曾經出現過的主機（`host.added` 的判斷）
    known: Mutex<HashSet<String>>,
}

impl HostIndex {
    /// 載入清冊中已有的主機（啟動時）
    pub fn load_known(&self, ids: impl IntoIterator<Item = String>) {
        let mut known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        known.extend(ids);
    }

    /// 記下這台主機；第一次出現時回傳 `true`
    pub fn mark_known(&self, agent_id: &str) -> bool {
        let mut known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        known.insert(agent_id.to_string())
    }

    /// 清冊（最多 [`DB_CACHE_MAX_AGE`] 讀一次；有新的主機寫入清冊時（最大的 host_id 改變）立即重新讀取，
    /// 例如由 `kairos reports import` 或第一筆 report 建立、尚未在狀態表中的主機）
    async fn db_hosts(&self, ctx: &ApiContext) -> Result<Arc<Vec<DbHost>>, ApiError> {
        let mut guard = self.db.lock().await;
        let max_id = |conn: &dyn Store| conn.max_host_id();
        if let Some((at, cached_max, hosts)) = guard.as_ref()
            && at.elapsed() < DB_CACHE_MAX_AGE
        {
            let current = ctx
                .pool
                .read("主機清單", move |conn| {
                    max_id(conn).map_err(query_failed("主機清單"))
                })
                .await?;
            if current == *cached_max {
                return Ok(hosts.clone());
            }
        }
        let (max, hosts) = ctx
            .pool
            .read("主機清單", move |conn| {
                let max = max_id(conn).map_err(query_failed("主機清單"))?;
                let hosts = conn.load_hosts().map_err(query_failed("主機清單"))?;
                Ok((max, hosts))
            })
            .await?;
        let hosts = Arc::new(hosts);
        *guard = Some((Instant::now(), max, hosts.clone()));
        Ok(hosts)
    }

    /// 目前的索引（最多 [`SNAPSHOT_MAX_AGE`] 舊）
    pub async fn snapshot(&self, ctx: &ApiContext) -> Result<Arc<Snapshot>, ApiError> {
        let mut guard = self.snapshot.lock().await;
        if let Some((at, snap)) = guard.as_ref()
            && at.elapsed() < SNAPSHOT_MAX_AGE
        {
            return Ok(snap.clone());
        }
        let db = self.db_hosts(ctx).await?;
        // 先取事件編號再取即時狀態：之後的事件一定比索引新
        let last_event_id = ctx.events.last_id();
        let live = ctx.registry.live_view();
        let rejections = crate::tls::rejections().all();
        let env = ctx.merge_env();
        let snap = Arc::new(build_snapshot(&db, live, &rejections, &env, last_event_id));
        *guard = Some((Instant::now(), snap.clone()));
        Ok(snap)
    }

    /// 清冊有變化時（例如寫入了被拒的憑證）讓下一次查詢重新讀取
    pub async fn invalidate_db(&self) {
        *self.db.lock().await = None;
    }

    /// 讓下一次查詢重新建立索引（測試與量測用）
    #[cfg(test)]
    pub async fn invalidate(&self) {
        *self.db.lock().await = None;
        *self.snapshot.lock().await = None;
    }
}

// ---- 篩選 ----

/// 搜尋（`q`）
#[derive(Debug, Clone, PartialEq)]
enum Search {
    Ip(IpAddr),
    Cidr(Cidr),
    Text {
        lower: String,
        /// 4 個字元以上、看起來像 agent_id 的前綴
        agent_prefix: Option<String>,
    },
}

/// 列表的篩選條件（`kairos/API.md` 第 5.1 節）
#[derive(Debug, Clone, Default)]
pub struct Filter {
    status: Option<Vec<Status>>,
    never_connected: Option<bool>,
    offline_for: Option<Duration>,
    offline_reason: Vec<String>,
    tags: Vec<String>,
    os: Vec<String>,
    versions: Vec<String>,
    durable: Option<bool>,
    subnets: Vec<Cidr>,
    search: Option<Search>,
    /// `group` 參數（小寫，取「或」）
    groups: Vec<String>,
    /// `os_family` 參數（`linux`、`windows`；取「或」）
    os_family: Vec<String>,
    /// 角色指派的範圍：只看得到這些主機群組（小寫）；`None` 為全部
    scope: Option<Vec<String>>,
    /// `config_state` 參數（集中設定的狀態；取「或」）
    config_state: Vec<String>,
    /// `remote_upgrade` 參數（`install.upgrade` 的值，沒有 `install` 的主機為 `none`；取「或」）
    remote_upgrade: Vec<String>,
}

/// `offline_reason` 參數接受的值（0.7 的名稱；舊名稱 `timeout`、`connection_closed` 在過渡期也接受，
/// 見 [`crate::registry::normalize_offline_reason`]）
const OFFLINE_REASONS: [&str; 6] = [
    "idle_timeout",
    "agent_closed",
    "server_shutdown",
    "certificate_rejected",
    "write_unknown",
    "server_lost",
];

fn parse_cidr(param: &str, text: &str) -> Result<Cidr, ApiError> {
    text.parse::<Cidr>().map_err(|e| {
        ApiError::param(
            param,
            format!("{param} 的值 \"{text}\" 不是合法的網段：{e}"),
        )
    })
}

impl Filter {
    pub fn parse(q: &Query) -> Result<Self, ApiError> {
        let mut f = Filter::default();
        let status = q.list("status");
        if !status.is_empty() {
            let mut list = Vec::new();
            for s in &status {
                list.push(Status::parse(s).ok_or_else(|| {
                    ApiError::param(
                        "status",
                        format!(
                            "status 的值 \"{s}\" 不是主機狀態 token（critical、offline、warning、unknown、pending、online）"
                        ),
                    )
                })?);
            }
            f.status = Some(list);
        }
        f.never_connected = q.bool("never_connected")?;
        if let Some(v) = q.get("offline_for") {
            f.offline_for = Some(parse_duration(v).ok_or_else(|| {
                ApiError::param(
                    "offline_for",
                    format!("offline_for 的值 \"{v}\" 無效，例如 90m、12h、3d、2w"),
                )
            })?);
        }
        for r in q.list("offline_reason") {
            let name = crate::registry::normalize_offline_reason(&r);
            if !OFFLINE_REASONS.contains(&name) {
                return Err(ApiError::param(
                    "offline_reason",
                    format!(
                        "offline_reason 的值 \"{r}\" 無效（{}；舊名稱 timeout、connection_closed 在 0.7 仍接受）",
                        OFFLINE_REASONS.join("、")
                    ),
                ));
            }
            f.offline_reason.push(name.to_string());
        }
        f.tags = q.list("tag").iter().map(|t| t.to_lowercase()).collect();
        f.os = q.list("os").iter().map(|t| t.to_lowercase()).collect();
        f.versions = q.list("version");
        f.groups = q.list("group").iter().map(|t| t.to_lowercase()).collect();
        for v in q.list("os_family") {
            let v = v.to_lowercase();
            if !matches!(v.as_str(), "linux" | "windows") {
                return Err(ApiError::param(
                    "os_family",
                    format!("os_family 的值 \"{v}\" 無效（linux、windows）"),
                ));
            }
            f.os_family.push(v);
        }
        for v in q.list("config_state") {
            if !crate::remote::desired::CONFIG_STATES.contains(&v.as_str()) {
                return Err(ApiError::param(
                    "config_state",
                    format!(
                        "config_state 的值 \"{v}\" 無效（{}）",
                        crate::remote::desired::CONFIG_STATES.join("、")
                    ),
                ));
            }
            f.config_state.push(v);
        }
        for v in q.list("remote_upgrade") {
            if v != "none" && !kairos_proto::mgmt::is_install_token(&v) {
                return Err(ApiError::param(
                    "remote_upgrade",
                    format!(
                        "remote_upgrade 的值 \"{v}\" 無效（ready、disabled、no_trust_anchor、no_helper、unsupported、none）"
                    ),
                ));
            }
            f.remote_upgrade.push(v);
        }
        f.durable = q.bool("durable")?;
        for s in q.list("subnet") {
            f.subnets.push(parse_cidr("subnet", &s)?);
        }
        if let Some(text) = q.get("q").map(str::trim) {
            f.search = Some(if let Ok(ip) = text.parse::<IpAddr>() {
                Search::Ip(ip.to_canonical())
            } else if text.contains('/')
                && text
                    .split('/')
                    .next()
                    .is_some_and(|a| a.parse::<IpAddr>().is_ok())
            {
                Search::Cidr(parse_cidr("q", text)?)
            } else {
                let lower = text.to_lowercase();
                let looks_like_id =
                    lower.len() >= 4 && lower.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
                Search::Text {
                    agent_prefix: looks_like_id.then(|| lower.clone()),
                    lower,
                }
            });
        }
        Ok(f)
    }

    /// 套用身分的主機範圍（群組範圍的角色只看得到自己的群組）
    pub fn with_access(mut self, access: &HostAccess) -> Self {
        self.scope = match access {
            HostAccess::All => None,
            HostAccess::Groups(g) => Some(g.clone()),
            HostAccess::None => Some(Vec::new()),
        };
        self
    }

    /// 狀態以外的條件
    fn matches_except_status(&self, h: &HostView, now_ms: i64) -> bool {
        let group = h.host_group.as_deref().map(|g| g.trim().to_lowercase());
        if let Some(scope) = &self.scope
            && !group.as_ref().is_some_and(|g| scope.contains(g))
        {
            return false;
        }
        if !self.groups.is_empty() && !group.as_ref().is_some_and(|g| self.groups.contains(g)) {
            return false;
        }
        if let Some(nc) = self.never_connected
            && h.never_connected != nc
        {
            return false;
        }
        if let Some(d) = self.offline_for {
            let need = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
            if h.status == Status::Online || now_ms - h.last_seen < need {
                return false;
            }
        }
        if !self.offline_reason.is_empty()
            && !h
                .offline_reason
                .is_some_and(|r| self.offline_reason.iter().any(|x| x == r))
        {
            return false;
        }
        if !self.tags.is_empty()
            && !h
                .tags
                .iter()
                .any(|t| self.tags.iter().any(|x| *x == t.to_lowercase()))
        {
            return false;
        }
        if !self.os.is_empty()
            && !h
                .os
                .as_ref()
                .and_then(|o| o.name.as_deref())
                .is_some_and(|n| self.os.iter().any(|x| *x == n.to_lowercase()))
        {
            return false;
        }
        if !self.os_family.is_empty() && !h.os_family().is_some_and(|f| self.os_family.contains(&f))
        {
            return false;
        }
        if !self.versions.is_empty()
            && !h
                .agent_version
                .as_deref()
                .is_some_and(|v| self.versions.iter().any(|x| x == v))
        {
            return false;
        }
        if let Some(d) = self.durable
            && h.durable != Some(d)
        {
            return false;
        }
        if !self.config_state.is_empty() && !self.config_state.iter().any(|s| s == h.config_state) {
            return false;
        }
        if !self.remote_upgrade.is_empty() {
            let v = h
                .install
                .as_ref()
                .and_then(|i| i.upgrade.as_deref())
                .unwrap_or("none");
            if !self.remote_upgrade.iter().any(|x| x == v) {
                return false;
            }
        }
        if !self.subnets.is_empty()
            && !h
                .ip
                .is_some_and(|ip| self.subnets.iter().any(|c| c.contains(ip)))
        {
            return false;
        }
        match &self.search {
            None => true,
            Some(Search::Ip(ip)) => h.ip == Some(*ip),
            Some(Search::Cidr(c)) => h.ip.is_some_and(|ip| c.contains(ip)),
            Some(Search::Text {
                lower,
                agent_prefix,
            }) => {
                h.name_lower.contains(lower.as_str())
                    || h.hostname_lower.contains(lower.as_str())
                    || agent_prefix
                        .as_deref()
                        .is_some_and(|p| h.agent_id.starts_with(p))
            }
        }
    }

    fn matches_status(&self, h: &HostView) -> bool {
        self.status.as_ref().is_none_or(|s| s.contains(&h.status))
    }
}

// ---- 排序與游標 ----

/// 排序鍵；`N` 為沒有值（一律排在最後）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Key {
    N,
    I(i64),
    F(f64),
    S(String),
    P(i64, i64),
    V(Vec<u64>, String),
}

impl Key {
    fn variant(&self) -> u8 {
        match self {
            Key::N => 0,
            Key::I(_) => 1,
            Key::F(_) => 2,
            Key::S(_) => 3,
            Key::P(..) => 4,
            Key::V(..) => 5,
        }
    }

    fn cmp_same(&self, other: &Key) -> Ordering {
        match (self, other) {
            (Key::I(a), Key::I(b)) => a.cmp(b),
            (Key::F(a), Key::F(b)) => a.total_cmp(b),
            (Key::S(a), Key::S(b)) => a.cmp(b),
            (Key::P(a1, a2), Key::P(b1, b2)) => (a1, a2).cmp(&(b1, b2)),
            (Key::V(a, sa), Key::V(b, sb)) => a.cmp(b).then_with(|| sa.cmp(sb)),
            _ => self.variant().cmp(&other.variant()),
        }
    }
}

/// 沒有值的一律在最後；其餘依方向
fn cmp_keys(a: &Key, b: &Key, desc: bool) -> Ordering {
    match (a, b) {
        (Key::N, Key::N) => Ordering::Equal,
        (Key::N, _) => Ordering::Greater,
        (_, Key::N) => Ordering::Less,
        _ => {
            let o = a.cmp_same(b);
            if desc { o.reverse() } else { o }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortField {
    Status,
    Hostname,
    LastSeen,
    FirstSeen,
    Ip,
    AgentVersion,
    Cpu,
    Memory,
    Disk,
    Uptime,
}

const SORT_FIELDS: [(&str, SortField); 10] = [
    ("status", SortField::Status),
    ("hostname", SortField::Hostname),
    ("last_seen", SortField::LastSeen),
    ("first_seen", SortField::FirstSeen),
    ("ip", SortField::Ip),
    ("agent_version", SortField::AgentVersion),
    ("cpu", SortField::Cpu),
    ("memory", SortField::Memory),
    ("disk", SortField::Disk),
    ("uptime", SortField::Uptime),
];

/// 排序方式（`sort` 參數）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    field: SortField,
    desc: bool,
    /// 原始的參數值（寫進游標，換排序時拒絕舊游標）
    text: String,
}

impl Sort {
    pub fn parse(text: Option<&str>) -> Result<Self, ApiError> {
        let text = text.unwrap_or("status").trim();
        let (desc, name) = match text.strip_prefix('-') {
            Some(n) => (true, n),
            None => (false, text),
        };
        let field = SORT_FIELDS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, f)| *f)
            .ok_or_else(|| {
                ApiError::param(
                    "sort",
                    format!(
                        "sort 的值 \"{text}\" 無效（{}，前面加 - 為遞減）",
                        SORT_FIELDS.map(|(n, _)| n).join("、")
                    ),
                )
            })?;
        Ok(Self {
            field,
            desc,
            text: text.to_string(),
        })
    }

    fn key(&self, h: &HostView) -> Key {
        let opt_f = |v: Option<f64>| v.map_or(Key::N, Key::F);
        match self.field {
            SortField::Status => Key::P(h.status.rank(), h.last_seen),
            SortField::Hostname => Key::S(h.name_lower.clone()),
            SortField::LastSeen => Key::I(h.last_seen),
            SortField::FirstSeen => Key::I(h.first_seen),
            SortField::Ip => match h.ip {
                Some(IpAddr::V4(v4)) => Key::S(format!("4{:032x}", u32::from(v4))),
                Some(IpAddr::V6(v6)) => Key::S(format!("6{:032x}", u128::from(v6))),
                None => Key::N,
            },
            SortField::AgentVersion => match h.agent_version.as_deref() {
                Some(v) => Key::V(version_numbers(v), v.to_string()),
                None => Key::N,
            },
            SortField::Cpu => opt_f(h.cpu),
            SortField::Memory => opt_f(h.mem_ratio),
            SortField::Disk => opt_f(h.top_disk().map(|d| d.1)),
            // 開機時間越早、經過時間越長；不依現在時間排序（游標在換頁之間穩定）
            SortField::Uptime => match (h.status == Status::Online)
                .then(|| h.boot_estimate())
                .flatten()
            {
                Some(b) => Key::I(-b),
                None => Key::N,
            },
        }
    }

    fn cmp(&self, a: (&Key, &str), b: (&Key, &str)) -> Ordering {
        cmp_keys(a.0, b.0, self.desc).then_with(|| a.1.cmp(b.1))
    }
}

/// `version`（SemVer）是否早於 `(major, minor)`；無法解析時 `false`
pub fn version_before(version: &str, (major, minor): (u64, u64)) -> bool {
    let n = version_numbers(version);
    match (n.first(), n.get(1)) {
        (Some(a), Some(b)) => (*a, *b) < (major, minor),
        _ => false,
    }
}

/// SemVer 的數字部分（`0.3.1-rc.1` → [0, 3, 1]）；無法解析的部分略過
fn version_numbers(v: &str) -> Vec<u64> {
    v.split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map_while(|p| p.parse().ok())
        .collect()
}

/// 游標的內容
#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    v: u8,
    s: String,
    k: Key,
    id: String,
}

fn encode_cursor(sort: &Sort, key: Key, id: &str) -> String {
    let c = Cursor {
        v: 1,
        s: sort.text.clone(),
        k: key,
        id: id.to_string(),
    };
    b64url_encode(&serde_json::to_vec(&c).unwrap_or_default())
}

fn decode_cursor(sort: &Sort, text: &str) -> Result<(Key, String), ApiError> {
    let invalid = |why: &str| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            format!("cursor 無效：{why}；請從第一頁重新開始"),
        )
    };
    let bytes = b64url_decode(text).ok_or_else(|| invalid("無法解析"))?;
    let c: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid("無法解析"))?;
    if c.v != 1 {
        return Err(invalid("版本不符"));
    }
    if c.s != sort.text {
        return Err(invalid("排序方式與產生游標時不同"));
    }
    Ok((c.k, c.id))
}

/// 列表的查詢結果
pub struct Page<'a> {
    pub items: Vec<&'a HostView>,
    pub total: usize,
    pub next_cursor: Option<String>,
    pub counts: HashMap<Status, u64>,
}

/// 篩選、排序、分頁
pub fn query<'a>(
    snap: &'a Snapshot,
    filter: &Filter,
    sort: &Sort,
    cursor: Option<&str>,
    limit: usize,
    now_ms: i64,
) -> Result<Page<'a>, ApiError> {
    let after = cursor.map(|c| decode_cursor(sort, c)).transpose()?;
    let mut counts: HashMap<Status, u64> = HashMap::new();
    let mut matched: Vec<(Key, &HostView)> = Vec::new();
    for h in &snap.hosts {
        if !filter.matches_except_status(h, now_ms) {
            continue;
        }
        *counts.entry(h.status).or_default() += 1;
        if filter.matches_status(h) {
            matched.push((sort.key(h), h));
        }
    }
    matched.sort_unstable_by(|a, b| sort.cmp((&a.0, &a.1.agent_id), (&b.0, &b.1.agent_id)));
    let total = matched.len();
    let start = match &after {
        Some((key, id)) => matched
            .partition_point(|(k, h)| sort.cmp((k, &h.agent_id), (key, id)) != Ordering::Greater),
        None => 0,
    };
    let end = (start + limit).min(total);
    let next_cursor = (end < total && end > start).then(|| {
        let (k, h) = &matched[end - 1];
        encode_cursor(sort, k.clone(), &h.agent_id)
    });
    Ok(Page {
        items: matched[start.min(total)..end]
            .iter()
            .map(|(_, h)| *h)
            .collect(),
        total,
        next_cursor,
        counts,
    })
}

const LIST_PARAMS: [&str; 17] = [
    "config_state",
    "remote_upgrade",
    "os_family",
    "status",
    "never_connected",
    "offline_for",
    "offline_reason",
    "tag",
    "os",
    "version",
    "durable",
    "subnet",
    "q",
    "sort",
    "limit",
    "cursor",
    "group",
];

/// `GET /api/v1/hosts`
pub async fn list(
    ctx: &Arc<ApiContext>,
    q: &Query,
    access: &HostAccess,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&LIST_PARAMS, &[])?;
    let filter = Filter::parse(q)?.with_access(access);
    let sort = Sort::parse(q.get("sort"))?;
    let limit = match q.get("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| ApiError::param("limit", format!("limit 必須介於 1～{MAX_LIMIT}")))?,
    };
    let snap = ctx.hosts.snapshot(ctx).await?;
    let now_ms = now.timestamp_millis();
    let page = query(&snap, &filter, &sort, q.get("cursor"), limit, now_ms)?;
    let mut data: Vec<Value> = page.items.iter().map(|h| h.summary(now_ms)).collect();
    // 建議的最低 Pulse 版本（版本庫中最新版本的清單 `min_upgrade_from`；REMOTE.md 第 4.9 節、R9）：低於它的
    // 主機標 `agent_outdated`（S04 版本欄的 ⚠）。只需要 `hosts:read`，不透露版本庫的其他內容
    let recommended = match &ctx.remote {
        Some(r) => r.recommended_min_agent_version().await,
        None => None,
    };
    let min = recommended
        .as_deref()
        .and_then(|v| kairos_proto::release::Version::parse(v).ok());
    for h in data.iter_mut() {
        let outdated = match (&min, h["agent_version"].as_str()) {
            (Some(min), Some(v)) => {
                kairos_proto::release::Version::parse(v).is_ok_and(|v| v < *min)
            }
            _ => false,
        };
        h["agent_outdated"] = json!(outdated);
    }
    let unsupported: Vec<&str> = Status::ALL
        .iter()
        .filter(|s| !s.supported())
        .map(|s| s.as_str())
        .collect();
    Ok(super::http::json(
        StatusCode::OK,
        now,
        json!({
            "data": data,
            "page": { "limit": limit, "total": page.total, "next_cursor": page.next_cursor },
            "meta": {
                "status_counts": status_counts(&page.counts),
                "unsupported_status": unsupported,
                "last_event_id": snap.last_event_id,
                "data_as_of": ts(snap.at),
                "recommended_min_agent_version": recommended,
            },
        }),
    ))
}

/// 篩選值的每一種最多列出幾個
pub const FACET_MAX: usize = 200;

/// 一種篩選值的計數：`(值, 台數, 其中線上的台數)`，依台數由多到少、再依值排序
fn facet_json(counts: HashMap<String, (u64, u64)>, with_online: bool) -> Value {
    let mut items: Vec<(String, (u64, u64))> = counts.into_iter().collect();
    items.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    let truncated = items.len() > FACET_MAX;
    items.truncate(FACET_MAX);
    let values: Vec<Value> = items
        .into_iter()
        .map(|(v, (n, online))| {
            if with_online {
                json!({ "value": v, "count": n, "online": online })
            } else {
                json!({ "value": v, "count": n })
            }
        })
        .collect();
    json!({ "values": values, "truncated": truncated })
}

/// `GET /api/v1/hosts/facets`：主機清單篩選列的選項（設計包 S04 的 OS、agent 版本、標籤、群組、
/// 離線原因下拉選單；S03 的群組健康）。只計算身分範圍內的主機，不受其他篩選條件影響
pub async fn facets(
    ctx: &Arc<ApiContext>,
    q: &Query,
    access: &HostAccess,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    q.reject_unknown(&[], &[])?;
    let snap = ctx.hosts.snapshot(ctx).await?;
    let mut os: HashMap<String, (u64, u64)> = HashMap::new();
    let mut version: HashMap<String, (u64, u64)> = HashMap::new();
    let mut tag: HashMap<String, (u64, u64)> = HashMap::new();
    let mut group: HashMap<String, (u64, u64)> = HashMap::new();
    let mut reason: HashMap<String, (u64, u64)> = HashMap::new();
    let mut family: HashMap<String, (u64, u64)> = HashMap::new();
    let mut total = 0u64;
    let mut no_group = (0u64, 0u64);
    let bump = |m: &mut HashMap<String, (u64, u64)>, k: &str, online: bool| {
        let e = m.entry(k.to_string()).or_default();
        e.0 += 1;
        e.1 += u64::from(online);
    };
    for h in snap
        .hosts
        .iter()
        .filter(|h| access.allows(h.host_group.as_deref()))
    {
        total += 1;
        let online = h.status == Status::Online;
        if let Some(name) = h.os.as_ref().and_then(|o| o.name.as_deref()) {
            bump(&mut os, name, online);
        }
        if let Some(v) = h.agent_version.as_deref() {
            bump(&mut version, v, online);
        }
        if let Some(f) = h.os_family() {
            bump(&mut family, &f, online);
        }
        for t in h.tags.iter() {
            bump(&mut tag, t, online);
        }
        match h.host_group.as_deref() {
            Some(g) => bump(&mut group, g, online),
            None => {
                no_group.0 += 1;
                no_group.1 += u64::from(online);
            }
        }
        if let Some(r) = h.offline_reason.filter(|_| !online) {
            bump(&mut reason, r, online);
        }
    }
    Ok(ok(
        now,
        json!({
            "total": total,
            "os": facet_json(os, false),
            "os_family": facet_json(family, false),
            "version": facet_json(version, false),
            "tag": facet_json(tag, false),
            "group": facet_json(group, true),
            "no_group": { "count": no_group.0, "online": no_group.1 },
            "offline_reason": facet_json(reason, false),
            "data_as_of": ts(snap.at),
        }),
    ))
}

/// `GET /api/v1/hosts/{agent_id}/connections`：這台主機最近的上線與離線（記憶體中，
/// [`super::conn_log`]）。範圍外或不存在的主機回 404
pub async fn connections(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    access: &HostAccess,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let id = parse_agent_id(agent_id)?;
    let q = id.clone();
    let exists_group: Option<Option<String>> = ctx
        .pool
        .read("主機詳情", move |conn| {
            conn.host_group_of(&q).map_err(query_failed("主機詳情"))
        })
        .await?;
    let known = exists_group.is_some() || ctx.registry.live_get(&id).is_some();
    let group = exists_group.flatten();
    if !known || !access.allows(group.as_deref()) {
        return Err(ApiError::host_not_found(&id));
    }
    let entries: Vec<Value> = ctx.conn_log.get(&id).iter().map(|e| e.json()).collect();
    Ok(ok(
        now,
        json!({
            "agent_id": id,
            "since": ts(ctx.started),
            "persisted": false,
            "max_entries": super::conn_log::PER_HOST,
            "entries": entries,
        }),
    ))
}

/// 這台主機最新一張憑證的摘要（不含序號與指紋：那些需要 `pki:read`，見 `/api/v1/pki/certificates`）
fn certificate_summary(conn: &dyn Store, agent_id: &str, now_ms: i64) -> StoreResult<Value> {
    let row = conn.latest_cert_of(agent_id)?;
    Ok(match row {
        None => Value::Null,
        Some((not_after, revoked_at)) => {
            let state = if revoked_at.is_some() {
                "revoked"
            } else if not_after <= now_ms {
                "expired"
            } else if not_after - now_ms <= 30 * 86_400_000 {
                "expiring"
            } else {
                "valid"
            };
            json!({ "state": state, "not_after": ts_ms(not_after), "revoked_at": revoked_at.map(ts_ms) })
        }
    })
}

/// 路徑中的 agent_id：UUID（不分大小寫）
pub fn parse_agent_id(text: &str) -> Result<String, ApiError> {
    let id = text.to_ascii_lowercase();
    if kairos_proto::is_uuid_text(&id) {
        Ok(id)
    } else {
        Err(ApiError::host_not_found(text))
    }
}

/// 單台主機的端點（趨勢、指標清單）：這台主機是否在身分的範圍內（不在範圍內時與不存在相同，回 404）
pub async fn check_access(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    access: &HostAccess,
) -> Result<(), ApiError> {
    if *access == HostAccess::All {
        return Ok(());
    }
    let id = parse_agent_id(agent_id)?;
    let q = id.clone();
    let group: Option<String> = ctx
        .pool
        .read("主機詳情", move |conn| {
            conn.host_group_of(&q)
                .map(Option::flatten)
                .map_err(query_failed("主機詳情"))
        })
        .await?;
    if access.allows(group.as_deref()) {
        Ok(())
    } else {
        Err(ApiError::host_not_found(&id))
    }
}

/// `GET /api/v1/hosts/{agent_id}`
pub async fn detail(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    access: &HostAccess,
    now: DateTime<Utc>,
) -> Result<Response<ApiBody>, ApiError> {
    let agent_id = parse_agent_id(agent_id)?;
    let id = agent_id.clone();
    let now_ms = now.timestamp_millis();
    let (db, certificate) = ctx
        .pool
        .read("主機詳情", move |conn| {
            let host = conn.load_host(&id).map_err(query_failed("主機詳情"))?;
            let cert = certificate_summary(conn, &id, now_ms).map_err(query_failed("主機詳情"))?;
            Ok((host, cert))
        })
        .await?;
    // 範圍外的主機與不存在的主機相同（不透露它存在）
    if !access.allows(db.as_ref().and_then(|d| d.host_group.as_deref())) {
        return Err(ApiError::host_not_found(&agent_id));
    }
    let live = ctx.registry.live_get(&agent_id);
    let rejection = crate::tls::rejections().get(&agent_id);
    let env = ctx.merge_env();
    let view = view_of(db.as_ref(), live.as_ref(), rejection.as_ref(), &env)
        .ok_or_else(|| ApiError::host_not_found(&agent_id))?;
    let mut data = view.summary(now_ms);
    data["host_fingerprint"] = json!(view.host_fingerprint.as_deref());
    data["peer"] = json!(view.peer.as_deref());
    data["connected_at"] = json!(view.connected_at.map(ts_ms));
    data["connection"] = match live.as_ref().filter(|l| l.seen_since_start) {
        Some(l) => json!({
            "online": l.online,
            "durable": l.durable,
            "replaying": l.replaying(env.now_i),
            "last_backfill_at": l.backfill.map(|b| ts(b.at_wall)),
            "last_backfill_lag_ms": l.backfill.map(|b| b.lag_ms),
        }),
        None => Value::Null,
    };
    data["last_report"] = match view.last_report.as_deref() {
        Some(r) => {
            let mut v = report_summary(r);
            v["recv_at"] = json!(r.recv_at);
            v["seq"] = json!(r.seq);
            v["metrics"] = json!(r.metrics);
            let d = &r.detail;
            v["filesystems"] = json!(
                d.filesystems
                    .iter()
                    .map(|f| json!({
                        "mountpoint": f.mountpoint,
                        "fstype": f.fstype,
                        "total": f.total,
                        "used": f.used,
                        "available": f.available,
                        "usage": f.usage,
                        "estimated": f.estimated,
                    }))
                    .collect::<Vec<_>>()
            );
            v["swap"] = match (d.swap_total, d.swap_used) {
                (None, None) => Value::Null,
                (t, u) => json!({ "total": t, "used": u, "usage": d.swap_percent() }),
            };
            v["load"] = if d.load_1m.is_some() || d.load_5m.is_some() || d.load_15m.is_some() {
                json!({ "1m": d.load_1m, "5m": d.load_5m, "15m": d.load_15m })
            } else {
                Value::Null
            };
            v["process_count"] = json!(d.process_count);
            v["memory_available_bytes"] = json!(d.memory_available);
            v["uptime_secs"] = json!(d.uptime_secs);
            let io = |list: &[crate::registry::IoLast], a: &str, b: &str| -> Vec<Value> {
                list.iter()
                    .map(|n| {
                        let mut o = json!({ "device": n.device });
                        o[a] = json!(n.rx_rate);
                        o[b] = json!(n.tx_rate);
                        o
                    })
                    .collect()
            };
            v["network"] = json!(io(&d.network, "rx_bytes_per_sec", "tx_bytes_per_sec"));
            v["disk_io"] = json!(io(&d.disks, "read_bytes_per_sec", "write_bytes_per_sec"));
            v
        }
        None => Value::Null,
    };
    data["inventory"] = json!(view.inventory.as_deref().map(|i| i.json(true)));
    // 設計包 §30.5 #7：S05 的 KRS-W2108、KRS-W4104 提示
    data["inventory_rejection"] = match live.as_ref().and_then(|l| l.inventory_rejected.clone()) {
        Some((at, reason)) => {
            json!({ "at": ts(at), "reason": &*reason, "product_code": "KRS-W2108" })
        }
        None => Value::Null,
    };
    data["series_limited_at"] = json!(db.as_ref().and_then(|d| d.series_limited_at).map(ts_ms));
    data["features"] = match live.as_ref().filter(|l| l.seen_since_start) {
        Some(l) => json!({ "inventory": l.inventory, "processes": l.processes }),
        None => Value::Null,
    };
    data["certificate_rejection"] = match &view.rejection {
        Some(r) => json!({
            "at": ts(r.at),
            "product_code": r.reason.product_code(),
            "protocol_code": "certificate_rejected",
            "reason": r.reason,
            "serial": r.serial,
            "peer": r.peer.map(|p| p.to_string()),
            "count": r.count,
        }),
        None => Value::Null,
    };
    // 0.7.0：最新一張憑證的有效狀態（S05「有效，至 …」；序號與指紋需要 pki:read）
    data["certificate"] = certificate;
    Ok(ok(now, data))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::registry::OfflineReason;
    use crate::tls::RejectionReason;

    pub fn db_host(id: &str, name: &str, peer: &str, last_seen: i64) -> DbHost {
        DbHost {
            agent_id: Arc::from(id),
            hostname: Arc::from(name),
            display_name: None,
            agent_version: Some(Arc::from("0.3.0")),
            proto: Some(2),
            host_fingerprint: None,
            tags: Arc::from(vec!["env:prod".to_string()]),
            os: Some(Arc::new(kairos_proto::OsInfo {
                name: Some("Ubuntu".into()),
                version: Some("24.04".into()),
                kernel: None,
                arch: Some("x86_64".into()),
            })),
            report_interval_ms: Some(5000),
            peer: Some(Arc::from(peer)),
            identity_verified: true,
            first_seen: 0,
            connected_at: Some(0),
            last_seen,
            offline_after_ms: Some(75_000),
            last_report: None,
            imported: false,
            rejection: None,
            host_group: None,
            boot_time: None,
            inventory: None,
            series_limited_at: None,
            install: None,
            config: None,
        }
    }

    pub fn live(id: &str, online: bool, last_seen: DateTime<Utc>) -> LiveAgent {
        LiveAgent {
            agent_id: Arc::from(id),
            hostname: Arc::from(format!("h-{id}")),
            agent_version: Arc::from("0.3.1"),
            proto: 2,
            host: None,
            peer: Arc::from("10.30.3.27:51544"),
            online,
            seen_since_start: true,
            connected_at: last_seen,
            last_seen,
            offline_after: Duration::from_secs(75),
            last_report: Some(Arc::new(LastReport {
                recv_at: ts(last_seen),
                seq: 1,
                ts: ts(last_seen),
                cpu_usage: Some(42.5),
                total_memory: Some(100),
                used_memory: Some(25),
                metrics: Some(17),
                detail: Default::default(),
            })),
            durable: Some(true),
            inventory: Some(true),
            processes: Some(true),
            assets: None,
            inventory_rejected: None,
            backfill: None,
            offline_reason: (!online).then_some(OfflineReason::Timeout),
            offline_for: None,
        }
    }

    fn env(now: DateTime<Utc>, uptime: Duration) -> MergeEnv {
        MergeEnv {
            now,
            now_i: Instant::now(),
            started: now - chrono::Duration::from_std(uptime).unwrap(),
            uptime,
            mtls: true,
        }
    }

    fn id(n: u32) -> String {
        format!("{n:08x}-0000-4000-8000-000000000000")
    }

    /// API.md §5.1、設計包 `ia.md` §5.1 的狀態暖機期：只有「最後一次訊息在這次啟動之前、這次啟動後
    /// 還沒連上」的主機在啟動後 2 分鐘內為 unknown
    #[test]
    fn unknown_only_within_the_status_warmup_for_hosts_last_seen_before_start() {
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let before_start = db_host(&id(1), "db-01", "10.0.0.1:1", now_ms - 3_600_000);
        let within = env(now, STARTUP_GRACE - Duration::from_secs(1));
        let v = view_of(Some(&before_start), None, None, &within).unwrap();
        assert_eq!((v.status, v.starting), (Status::Unknown, true));
        // 2 分鐘後仍未連上：離線
        let after = env(now, STARTUP_GRACE);
        let v = view_of(Some(&before_start), None, None, &after).unwrap();
        assert_eq!((v.status, v.starting), (Status::Offline, false));
        // 最後一次訊息在這次啟動之後（清冊已更新、即時狀態已移除）：暖機期內也是離線
        let since_start = db_host(&id(2), "db-02", "10.0.0.2:1", now_ms - 1_000);
        let v = view_of(Some(&since_start), None, None, &within).unwrap();
        assert_eq!((v.status, v.starting), (Status::Offline, false));
        // 這次啟動後連上又中斷：暖機期內照常離線
        let l = live(&id(1), false, now - chrono::Duration::seconds(30));
        let v = view_of(Some(&before_start), Some(&l), None, &within).unwrap();
        assert_eq!((v.status, v.starting), (Status::Offline, false));
    }

    #[test]
    fn statuses_are_merged_from_inventory_and_live_state() {
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let long = env(now, Duration::from_secs(3600));
        let short = env(now, Duration::from_secs(30));
        let d = db_host(&id(1), "db-01", "10.0.0.1:1", now_ms - 86_400_000 * 2);
        // 只在清冊中：離線；剛啟動時為 unknown（starting）
        let v = view_of(Some(&d), None, None, &long).unwrap();
        assert_eq!(v.status, Status::Offline);
        assert_eq!(v.offline_for_secs(now_ms), Some(2 * 86_400));
        assert_eq!(v.summary(now_ms)["status_detail"]["long_offline"], true);
        let v = view_of(Some(&d), None, None, &short).unwrap();
        assert_eq!((v.status, v.starting), (Status::Unknown, true));
        // 匯入、從未連線
        let mut imported = d.clone();
        imported.imported = true;
        imported.connected_at = None;
        let v = view_of(Some(&imported), None, None, &long).unwrap();
        assert_eq!((v.status, v.never_connected), (Status::Unknown, true));
        assert_eq!(v.summary(now_ms)["source"], "import");
        // 線上：即時狀態蓋過清冊
        let l = live(&id(1), true, now);
        let v = view_of(Some(&d), Some(&l), None, &short).unwrap();
        assert_eq!(v.status, Status::Online);
        assert_eq!(v.offline_for_secs(now_ms), None);
        assert_eq!(v.agent_version.as_deref(), Some("0.3.1"));
        assert_eq!(v.ip, Some("10.30.3.27".parse().unwrap()));
        let s = v.summary(now_ms);
        assert_eq!(s["durable"], true);
        assert_eq!(s["replaying"], false);
        assert_eq!(s["last_report"]["cpu_usage"], 42.5);
        assert_eq!(s["last_report"]["memory_used_bytes"], 25 * 1024 * 1024);
        assert_eq!(s["tags"][0], "env:prod");
        assert_eq!(s["os"]["name"], "Ubuntu");
        assert_eq!(s["pseudonymized"], false);
        // 假名化的主機名稱（privacy.hostname = "pseudonymize"）
        let mut p = d.clone();
        p.hostname = "host-k3m7x2q7v4ab".into();
        let v = view_of(Some(&p), None, None, &long).unwrap();
        assert_eq!(v.summary(now_ms)["pseudonymized"], true);
        // 這次啟動後離線：帶原因
        let l = live(&id(1), false, now - chrono::Duration::seconds(90));
        let v = view_of(Some(&d), Some(&l), None, &short).unwrap();
        assert_eq!(v.status, Status::Offline);
        assert_eq!(v.offline_reason, Some("idle_timeout"));
        let s = v.summary(now_ms);
        assert_eq!(s["status_detail"]["offline_reason"], "idle_timeout");
        assert_eq!(s["status_detail"]["offline_reason_legacy"], "timeout");
        // 之後憑證被拒：原因改為 certificate_rejected
        let r = CertRejection {
            at: now,
            reason: RejectionReason::Revoked,
            serial: Some("1000".into()),
            peer: None,
            count: 3,
        };
        let v = view_of(Some(&d), Some(&l), Some(&r), &short).unwrap();
        assert_eq!(v.offline_reason, Some("certificate_rejected"));
        // 尚未寫入清冊的新主機
        let v = view_of(None, Some(&live(&id(9), true, now)), None, &long).unwrap();
        assert_eq!(v.first_seen, now_ms);
        assert!(v.identity_verified);
        assert!(view_of(None, None, None, &long).is_none());
    }

    fn snapshot(n: u32, now: DateTime<Utc>) -> Snapshot {
        let now_ms = now.timestamp_millis();
        let mut db = Vec::new();
        let mut lives = Vec::new();
        for i in 0..n {
            let mut d = db_host(
                &id(i),
                &format!("host-{i:05}"),
                &format!("10.{}.{}.{}:5000", i / 65536 % 256, i / 256 % 256, i % 256),
                now_ms - i64::from(i) * 60_000,
            );
            if i % 10 == 0 {
                d.tags = Arc::from(vec!["line3".to_string()]);
            }
            db.push(d);
            if i % 3 != 0 {
                let mut l = live(&id(i), true, now);
                l.hostname = Arc::from(format!("host-{i:05}"));
                l.peer = Arc::from(format!(
                    "10.{}.{}.{}:5000",
                    i / 65536 % 256,
                    i / 256 % 256,
                    i % 256
                ));
                lives.push(l);
            }
        }
        build_snapshot(
            &db,
            lives,
            &HashMap::new(),
            &env(now, Duration::from_secs(3600)),
            "x-0".into(),
        )
    }

    fn q(text: &str) -> Query {
        Query::parse(Some(text)).unwrap()
    }

    #[test]
    fn filters_sort_and_count() {
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let snap = snapshot(300, now);
        let run = |text: &str| {
            let qq = q(text);
            let f = Filter::parse(&qq).unwrap();
            let s = Sort::parse(qq.get("sort")).unwrap();
            query(&snap, &f, &s, None, 1000, now_ms).unwrap().total
        };
        assert_eq!(run(""), 300);
        assert_eq!(run("status=offline"), 100);
        assert_eq!(run("status=online"), 200);
        assert_eq!(run("status=online,offline"), 300);
        assert_eq!(run("status=critical"), 0);
        // 離線且超過 3 小時：i 為 3 的倍數且 i * 1 分鐘 ≥ 180 分鐘
        assert_eq!(
            run("offline_for=3h"),
            (180..300).filter(|i| i % 3 == 0).count()
        );
        assert_eq!(run("tag=LINE3"), 30);
        assert_eq!(run("os=ubuntu"), 300);
        assert_eq!(run("os=windows"), 0);
        assert_eq!(run("version=0.3.0"), 100);
        assert_eq!(run("version=0.3.1,0.3.0"), 300);
        assert_eq!(run("durable=true"), 200);
        assert_eq!(run("subnet=10.0.0.0/25"), 128);
        assert_eq!(run("subnet=10.0.0.0/25,10.0.1.0/24"), 128 + 44);
        assert_eq!(run("q=10.0.0.7"), 1);
        assert_eq!(run("q=10.0.1.0/24"), 44);
        assert_eq!(run("q=host-0001"), 10);
        assert_eq!(run("q=HOST-00042"), 1);
        assert_eq!(run(&format!("q={}", &id(42)[..8])), 1);
        assert_eq!(run("q=0000002a"), 1);
        assert_eq!(run("offline_reason=timeout"), 0);
        // 0.7 的名稱與舊名稱都接受，結果相同
        assert_eq!(run("offline_reason=idle_timeout"), 0);
        assert_eq!(run("offline_reason=write_unknown"), 0);
        // 狀態計數不受 status 篩選影響
        let qq = q("status=offline&tag=line3");
        let page = query(
            &snap,
            &Filter::parse(&qq).unwrap(),
            &Sort::parse(None).unwrap(),
            None,
            10,
            now_ms,
        )
        .unwrap();
        assert_eq!(page.total, 10);
        assert_eq!(page.counts.get(&Status::Online), Some(&20));
        assert_eq!(page.counts.get(&Status::Offline), Some(&10));
        let counts = status_counts(&page.counts);
        assert_eq!(counts["online"], 20);
        assert_eq!(counts["critical"], Value::Null);
        // 預設排序：離線在前、離線最久的在前
        let page = query(
            &snap,
            &Filter::default(),
            &Sort::parse(None).unwrap(),
            None,
            3,
            now_ms,
        )
        .unwrap();
        assert_eq!(&*page.items[0].agent_id, id(297));
        assert_eq!(&*page.items[1].agent_id, id(294));
        // 錯誤的參數
        for bad in [
            "status=down",
            "offline_for=soon",
            "offline_reason=bored",
            "durable=yes",
            "subnet=10.0.0.1/24",
            "q=10.0.0.0/33",
        ] {
            assert!(Filter::parse(&q(bad)).is_err(), "{bad}");
        }
        assert!(Sort::parse(Some("-color")).is_err());
    }

    #[test]
    fn cursor_pagination_visits_every_host_once() {
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let snap = snapshot(1050, now);
        for sort_text in [
            "status",
            "-last_seen",
            "hostname",
            "-ip",
            "cpu",
            "agent_version",
        ] {
            let sort = Sort::parse(Some(sort_text)).unwrap();
            let filter = Filter::default();
            let mut seen = HashSet::new();
            let mut cursor: Option<String> = None;
            let mut pages = 0;
            loop {
                let page = query(&snap, &filter, &sort, cursor.as_deref(), 100, now_ms).unwrap();
                assert_eq!(page.total, 1050);
                for h in &page.items {
                    assert!(
                        seen.insert(h.agent_id.to_string()),
                        "{sort_text}: 重複 {}",
                        h.agent_id
                    );
                }
                pages += 1;
                match page.next_cursor {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
            }
            assert_eq!(seen.len(), 1050, "{sort_text}");
            assert_eq!(pages, 11, "{sort_text}");
        }
        // 換排序方式時拒絕舊游標
        let sort = Sort::parse(Some("hostname")).unwrap();
        let page = query(&snap, &Filter::default(), &sort, None, 10, now_ms).unwrap();
        let other = Sort::parse(Some("-hostname")).unwrap();
        let err = query(
            &snap,
            &Filter::default(),
            &other,
            page.next_cursor.as_deref(),
            10,
            now_ms,
        )
        .err()
        .unwrap();
        assert_eq!(err.code, "invalid_cursor");
        assert_eq!(
            query(&snap, &Filter::default(), &sort, Some("!!!"), 10, now_ms)
                .err()
                .unwrap()
                .code,
            "invalid_cursor"
        );
    }

    #[test]
    fn version_and_ip_ordering() {
        assert_eq!(version_numbers("0.3.1"), [0, 3, 1]);
        assert_eq!(version_numbers("0.10.0-rc.1"), [0, 10, 0]);
        assert!(version_numbers("dev").is_empty());
        assert!(
            Key::V(version_numbers("0.10.0"), "0.10.0".into())
                .cmp_same(&Key::V(version_numbers("0.9.9"), "0.9.9".into()))
                == Ordering::Greater
        );
        // 沒有值的一律在最後
        assert_eq!(cmp_keys(&Key::N, &Key::I(1), false), Ordering::Greater);
        assert_eq!(cmp_keys(&Key::N, &Key::I(1), true), Ordering::Greater);
        assert_eq!(cmp_keys(&Key::I(2), &Key::I(1), true), Ordering::Less);
        assert_eq!(
            ip_of("[::ffff:10.0.0.1]:5"),
            Some("10.0.0.1".parse().unwrap())
        );
        assert_eq!(ip_of("10.0.0.1"), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(ip_of("weird"), None);
    }
}
