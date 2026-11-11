//! Kairos 內部的事件匯流排（設計包 `kairos/screens.md` §24 #2、#8；`kairos/ALERTS.md` 第 2 節）。
//!
//! 各元件把「發生了什麼」以結構化的 [`EventKind`] 發布到 [`EventBus`]；告警引擎（[`crate::alerts`]）
//! 是第一個訂閱者，之後的 `/metrics`、稽核外送與主控台都可以訂閱同一個匯流排，不必各自在發生處加程式碼。
//!
//! - 發布不會阻塞、不會失敗：底層是 `tokio::sync::broadcast`，沒有訂閱者時直接丟棄；訂閱者落後超過
//!   [`BUS_CAPACITY`] 則時收到 `Lagged`（遺失的事件以 [`EventBus::dropped`] 計數）。因此**事件只是提示**：
//!   需要精確狀態的訂閱者（例如告警引擎的主機離線判定）另外定期讀取狀態表，不只依賴事件。
//! - 發布點刻意放在「狀態已改變之後」：訂閱者收到事件時讀取狀態表，看到的一定是改變後的狀態。
//! - report 的寫入路徑（每秒數千筆）不逐筆發布；寫入失敗等以計數保存（`stats::WriteStats`），由告警引擎
//!   的系統檢查定期讀取，轉成 [`EventKind::Condition`] 的狀態變化再發布（只在變化時發布，數量很少）。

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;

use crate::registry::OfflineReason;

/// 每個訂閱者最多落後的事件數
pub const BUS_CAPACITY: usize = 8192;

/// 斷線原因的分類（設計包 §24 #2；Webhook 的 `offline_reason` 固定代碼，`alerts-ux.md` §6.6）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    /// 超過離線門檻沒有收到心跳（`offline_after_ms`）
    IdleTimeout,
    /// agent 關閉連線、網路中斷（TCP 結束或重設）
    AgentClosed,
    /// 用戶端憑證被拒（已撤銷、過期、不再受信任）
    CertificateRejected,
    /// 伺服器無法判定撤銷狀態（CRL 過期或缺漏；伺服器端的問題）。Webhook 的固定代碼之一；
    /// 狀態表目前把這類斷線記為 `CertificateRejected`（之後區分）
    #[allow(dead_code)]
    RevocationUnavailable,
    /// hello 的 agent_id 與用戶端憑證不符（hello 前就被拒，不是離線原因；以 `EventKind::AuthFailed` 發布）
    #[allow(dead_code)]
    AuthFailed,
    /// 同一個 agent_id 有新連線，舊連線被取代（主機仍在線上；以 `EventKind::HostSuperseded` 發布）
    #[allow(dead_code)]
    Superseded,
    /// Kairos 關機（這類離線永遠不產生告警事件，`alerts-ux.md` §3.3 B）
    ServerShutdown,
    /// report 寫入結果未知，Kairos 關閉了連線
    WriteUnknown,
    /// 服務這台主機的叢集節點失聯（`kairos/HA.md` 第 6.6 節；0.9.0 只定義名稱，叢集模式才會產生）
    ServerLost,
}

impl DisconnectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DisconnectReason::IdleTimeout => "idle_timeout",
            DisconnectReason::AgentClosed => "agent_closed",
            DisconnectReason::CertificateRejected => "certificate_rejected",
            DisconnectReason::RevocationUnavailable => "revocation_unavailable",
            DisconnectReason::AuthFailed => "auth_failed",
            DisconnectReason::Superseded => "superseded",
            DisconnectReason::ServerShutdown => "server_shutdown",
            DisconnectReason::WriteUnknown => "write_unknown",
            DisconnectReason::ServerLost => "server_lost",
        }
    }

    /// 中文說明在前、原始代碼括號附後（`alerts-ux.md` §6.1）
    pub fn label(self) -> &'static str {
        match self {
            DisconnectReason::IdleTimeout => "連線逾時（未收到心跳）",
            DisconnectReason::AgentClosed => "agent 關閉連線或網路中斷",
            DisconnectReason::CertificateRejected => "憑證被拒",
            DisconnectReason::RevocationUnavailable => "伺服器無法判定撤銷狀態（CRL）",
            DisconnectReason::AuthFailed => "身分驗證失敗",
            DisconnectReason::Superseded => "被同一 agent_id 的新連線取代",
            DisconnectReason::ServerShutdown => "Kairos 關機",
            DisconnectReason::WriteUnknown => "寫入結果未知",
            DisconnectReason::ServerLost => "叢集節點失聯",
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn parse(s: &str) -> Option<Self> {
        [
            DisconnectReason::IdleTimeout,
            DisconnectReason::AgentClosed,
            DisconnectReason::CertificateRejected,
            DisconnectReason::RevocationUnavailable,
            DisconnectReason::AuthFailed,
            DisconnectReason::Superseded,
            DisconnectReason::ServerShutdown,
            DisconnectReason::WriteUnknown,
            DisconnectReason::ServerLost,
        ]
        .into_iter()
        .find(|r| r.as_str() == s)
    }

    /// 狀態表的離線原因 → 分類
    pub fn from_offline(r: OfflineReason) -> Self {
        match r {
            OfflineReason::Timeout => DisconnectReason::IdleTimeout,
            OfflineReason::ConnectionClosed => DisconnectReason::AgentClosed,
            OfflineReason::ServerShutdown => DisconnectReason::ServerShutdown,
            OfflineReason::CertificateRejected => DisconnectReason::CertificateRejected,
            OfflineReason::WriteUnknown => DisconnectReason::WriteUnknown,
            OfflineReason::ServerLost => DisconnectReason::ServerLost,
        }
    }
}

/// Kairos 本身的生命週期
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// 已開始接受連線
    Started,
    /// 收到關機訊號
    ShuttingDown,
}

fn ser_arc<S: serde::Serializer>(v: &Arc<str>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(v)
}

fn ser_time<S: serde::Serializer>(v: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&crate::storage::fmt_time(*v))
}

/// 事件的內容
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// 主機上線、重新上線、離線後恢復
    HostOnline {
        #[serde(serialize_with = "ser_arc")]
        agent_id: Arc<str>,
        #[serde(serialize_with = "ser_arc")]
        hostname: Arc<str>,
        ip: Option<IpAddr>,
    },
    /// 主機離線（原因分類見 [`DisconnectReason`]）
    HostOffline {
        #[serde(serialize_with = "ser_arc")]
        agent_id: Arc<str>,
        #[serde(serialize_with = "ser_arc")]
        hostname: Arc<str>,
        ip: Option<IpAddr>,
        reason: DisconnectReason,
    },
    /// 同一個 agent_id 的新連線取代了仍在的舊連線（身分衝突的訊號，`KRS-W3102`）
    HostSuperseded {
        #[serde(serialize_with = "ser_arc")]
        agent_id: Arc<str>,
        previous_ip: Option<IpAddr>,
        ip: Option<IpAddr>,
    },
    /// hello 的 agent_id 與用戶端憑證不符（`KRS-W1102`）
    AuthFailed { agent_id: String, ip: IpAddr },
    /// 狀態表已滿，淘汰了離線最久的主機（`KRS-E3201`）
    RegistryEvicted { agent_id: String, hostname: String },
    /// 主機經 8443 自行除役（`deregister`）
    HostDecommissioned {
        agent_id: String,
        ip: Option<IpAddr>,
        initiated_by: Option<String>,
        already: bool,
    },
    /// 主機重新開機（`system.uptime` 變小；METRICS.md 第 11.2 節）。`boot_time` 為推算的開機時間
    HostRebooted {
        agent_id: String,
        hostname: String,
        uptime_secs: u64,
        previous_uptime_secs: u64,
        boot_time: Option<String>,
    },
    /// TLS 設定（憑證、CA、CRL）重新載入的結果；`crl`：變更涉及 CRL（失敗時 `KRS-E1204`）
    TlsReload {
        ok: bool,
        crl: bool,
        message: Option<String>,
    },
    /// 系統檢查的狀態變化（告警引擎發布；`active` 由 false 變 true 或反之，或嚴重度改變）
    Condition {
        check: String,
        subject: String,
        active: bool,
        severity: Option<&'static str>,
        code: Option<&'static str>,
        summary: String,
    },
    /// Kairos 的生命週期
    Lifecycle { phase: Lifecycle },
    /// 通知管道的機密無法解開（`available = false`，`KRS-E5110`）或已恢復；告警服務在狀態轉換時發布
    ChannelSecrets {
        channel_id: String,
        name: String,
        available: bool,
    },
    /// 稽核日誌的定期自我驗證（[`crate::audit::monitor`]）：`ok = false` 時 `KRS-E6201`
    /// （`reason` 為斷點的原因，`id` 為斷點的紀錄）。每次驗證都發布
    AuditIntegrity {
        ok: bool,
        reason: Option<String>,
        id: Option<i64>,
        summary: String,
    },
    /// 遠端管理的系統檢查（升級推送已暫停、回滾失敗、發佈簽章驗證失敗；REMOTE.md 第 4.7.4 節）。
    /// `rule` 為系統規則的 ID，`active = false` 時恢復。由遠端管理服務每一輪依資料庫的狀態調和後發布
    RemoteCondition {
        rule: &'static str,
        subject: String,
        active: bool,
        title: String,
        details: serde_json::Value,
    },
    /// 升級中主機的離線告警抑制（REMOTE.md 第 4.7.5 節）：推送中的主機進入 `applying`（已交給輔助程式）時
    /// `until_ms` 為抑制的期限（`applying_at + health_timeout_secs + 10 分鐘`）；到達終止狀態或期限已過時
    /// `None`。由遠端管理服務在狀態改變的當下與每一輪依資料庫調和後發布
    UpgradeWindow {
        agent_id: String,
        rollout_id: String,
        until_ms: Option<i64>,
    },
}

impl EventKind {
    /// 事件種類的名稱（日誌與訂閱者過濾）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::HostOnline { .. } => "host_online",
            EventKind::HostOffline { .. } => "host_offline",
            EventKind::HostSuperseded { .. } => "host_superseded",
            EventKind::AuthFailed { .. } => "auth_failed",
            EventKind::RegistryEvicted { .. } => "registry_evicted",
            EventKind::HostDecommissioned { .. } => "host_decommissioned",
            EventKind::HostRebooted { .. } => "host_rebooted",
            EventKind::TlsReload { .. } => "tls_reload",
            EventKind::Condition { .. } => "condition",
            EventKind::Lifecycle { .. } => "lifecycle",
            EventKind::ChannelSecrets { .. } => "channel_secrets",
            EventKind::AuditIntegrity { .. } => "audit_integrity",
            EventKind::RemoteCondition { .. } => "remote_condition",
            EventKind::UpgradeWindow { .. } => "upgrade_window",
        }
    }
}

/// 一則事件
#[derive(Debug, Clone, Serialize)]
pub struct BusEvent {
    /// 這次啟動以來的序號（從 1 起）
    pub seq: u64,
    #[serde(serialize_with = "ser_time")]
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// 事件匯流排
#[derive(Debug)]
pub struct EventBus {
    tx: broadcast::Sender<Arc<BusEvent>>,
    seq: AtomicU64,
    dropped: AtomicU64,
}

impl Default for EventBus {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(BUS_CAPACITY).0,
            seq: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }
}

impl EventBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 發布一則事件（時間為現在）；回傳序號
    pub fn publish(&self, kind: EventKind) -> u64 {
        self.publish_at(Utc::now(), kind)
    }

    /// 發布一則事件（指定時間）
    pub fn publish_at(&self, at: DateTime<Utc>, kind: EventKind) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        if self.tx.receiver_count() > 0 {
            let _ = self.tx.send(Arc::new(BusEvent { seq, at, kind }));
        }
        seq
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<BusEvent>> {
        self.tx.subscribe()
    }

    /// 已發布的事件數
    #[allow(dead_code)]
    pub fn published(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// 訂閱者回報落後而遺失的事件數（[`EventBus::note_lagged`]）
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 訂閱者收到 `Lagged(n)` 時呼叫
    pub fn note_lagged(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }
}

/// `ip:port` 形式的 peer → IP（無法解析時為 `None`）
pub fn peer_ip(peer: &str) -> Option<IpAddr> {
    peer.parse::<std::net::SocketAddr>()
        .map(|a| a.ip().to_canonical())
        .ok()
        .or_else(|| peer.parse::<IpAddr>().ok().map(|ip| ip.to_canonical()))
        // privacy.store_peer_ip = "truncate"：網段（`10.0.3.0/24`）以網段的位址代表
        .or_else(|| {
            peer.split_once('/')
                .and_then(|(ip, _)| ip.parse::<IpAddr>().ok())
                .map(|ip| ip.to_canonical())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_reach_subscribers_in_order_and_without_subscribers_are_dropped() {
        let bus = EventBus::new();
        // 沒有訂閱者：照常編號，不會失敗
        assert_eq!(
            bus.publish(EventKind::Lifecycle {
                phase: Lifecycle::Started
            }),
            1
        );
        let mut rx = bus.subscribe();
        bus.publish(EventKind::AuthFailed {
            agent_id: "a".into(),
            ip: "192.0.2.1".parse().unwrap(),
        });
        bus.publish(EventKind::HostOffline {
            agent_id: "a".into(),
            hostname: "h".into(),
            ip: None,
            reason: DisconnectReason::IdleTimeout,
        });
        let first = rx.recv().await.unwrap();
        assert_eq!(first.seq, 2);
        assert_eq!(first.kind.name(), "auth_failed");
        let second = rx.recv().await.unwrap();
        assert_eq!(second.seq, 3);
        let v = serde_json::to_value(&*second).unwrap();
        assert_eq!(v["type"], "host_offline");
        assert_eq!(v["reason"], "idle_timeout");
        assert_eq!(bus.published(), 3);
    }

    #[test]
    fn reason_codes_round_trip_and_map_from_the_registry() {
        for r in [
            DisconnectReason::IdleTimeout,
            DisconnectReason::AgentClosed,
            DisconnectReason::CertificateRejected,
            DisconnectReason::RevocationUnavailable,
            DisconnectReason::AuthFailed,
            DisconnectReason::Superseded,
            DisconnectReason::ServerShutdown,
            DisconnectReason::WriteUnknown,
        ] {
            assert_eq!(DisconnectReason::parse(r.as_str()), Some(r));
            assert!(!r.label().is_empty());
        }
        assert_eq!(
            DisconnectReason::from_offline(OfflineReason::Timeout),
            DisconnectReason::IdleTimeout
        );
        assert_eq!(
            DisconnectReason::from_offline(OfflineReason::ConnectionClosed),
            DisconnectReason::AgentClosed
        );
        assert_eq!(
            DisconnectReason::from_offline(OfflineReason::WriteUnknown),
            DisconnectReason::WriteUnknown
        );
        assert_eq!(peer_ip("10.0.0.1:5000"), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(
            peer_ip("[::ffff:10.0.0.1]:5000"),
            Some("10.0.0.1".parse().unwrap())
        );
        assert_eq!(peer_ip("bad"), None);
    }
}
