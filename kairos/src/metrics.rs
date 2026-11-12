//! 執行中的計數器（審計 O3；設計包 `kairos/screens.md` S20、§24 #8）：`/metrics` 與 `/readyz` 讀取。
//!
//! 只有單純的原子計數器與直方圖（[`kp_obs::prom`]），由各處以一行呼叫記錄；計數不經過鎖，
//! 不影響 report 的 `ack` 延遲。已在其他模組計數的項目（report 寫入結果 [`crate::stats::WriteStats`]、
//! 被拒的連線 `per_ip::stats`、註冊端點 `enroll::EnrollStats`）不重複，在輸出時直接讀取。
//!
//! 標籤只用有限集合的值（原因、結果、路由的分類）；不以 agent_id、IP 或路徑當標籤。
//!
//! **與內部事件匯流排（[`crate::bus`]）的分工**：主機的上線、離線（依原因）、被取代、hello 的身分驗證
//! 失敗、狀態表淘汰只由 [`watch_bus`] 訂閱匯流排計數（與告警引擎看到的是同一組事件，計數不重複、
//! 不在連線的熱路徑上加程式碼）。匯流排上沒有的項目（每 IP／速率的拒絕、狀態表已滿、憑證被拒的原因、
//! 主控台 API）維持在發生處以一行計數。告警另由 [`watch_alerts`] 與通知的派送（`alerts::notify`）計數。

use kp_obs::prom::Counter;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast::error::RecvError;

use crate::bus::{BusEvent, DisconnectReason, EventBus, EventKind};
use crate::shutdown::Shutdown;
use crate::tls::RejectionReason;

/// 斷線（離線）原因的標籤值，順序與 [`offline_index`] 相同。與事件匯流排、告警與 Webhook 的
/// `offline_reason` 固定代碼相同（[`DisconnectReason::as_str`]；設計包 `decisions/INTEGRATION.md` §28.5 #4）
pub const OFFLINE_REASONS: [&str; 6] = [
    "idle_timeout",
    "agent_closed",
    "server_shutdown",
    "certificate_rejected",
    "write_unknown",
    "server_lost",
];

/// 匯流排的斷線原因 → 標籤（被取代、身分驗證失敗不是離線，回 `None`）
fn offline_index(r: DisconnectReason) -> Option<usize> {
    Some(match r {
        DisconnectReason::IdleTimeout => 0,
        DisconnectReason::AgentClosed => 1,
        DisconnectReason::ServerShutdown => 2,
        DisconnectReason::CertificateRejected | DisconnectReason::RevocationUnavailable => 3,
        DisconnectReason::WriteUnknown => 4,
        DisconnectReason::ServerLost => 5,
        DisconnectReason::AuthFailed | DisconnectReason::Superseded => return None,
    })
}

/// 告警事件的嚴重度（標籤），順序與 `alerts::model::Severity::ALL` 相同
pub const ALERT_SEVERITIES: [&str; 4] = ["emergency", "critical", "warning", "info"];

fn severity_index(s: &str) -> Option<usize> {
    ALERT_SEVERITIES.iter().position(|x| *x == s)
}

/// 通知管道的種類（標籤）
pub const CHANNEL_KINDS: [&str; 3] = ["email", "webhook", "other"];

/// 一次發送（SMTP 或 HTTP 請求）的結果
pub const SEND_RESULTS: [&str; 2] = ["ok", "error"];

/// 一則通知最後的狀態（`sent`：送出；`failed`：重試超過 15 分鐘或暫緩超過 24 小時；`dropped`：管道
/// 已停用或刪除）
pub const DELIVERY_STATES: [&str; 3] = ["sent", "failed", "dropped"];

/// 握手或重新驗證時被拒的用戶端憑證（[`RejectionReason`]）的標籤值
pub const CERT_REJECTION_REASONS: [&str; 5] = [
    "revoked",
    "expired",
    "unknown_issuer",
    "revocation_status_unavailable",
    "other",
];

fn cert_index(r: RejectionReason) -> usize {
    match r {
        RejectionReason::Revoked => 0,
        RejectionReason::Expired => 1,
        RejectionReason::UnknownIssuer => 2,
        RejectionReason::RevocationStatusUnavailable => 3,
        RejectionReason::Other => 4,
    }
}

/// 主控台 API 的路由分類（`/metrics` 的 `route` 標籤）
pub const API_ROUTES: [&str; 21] = [
    "me",
    "hosts",
    "metrics",
    "overview",
    "health",
    "events",
    "enroll",
    "pki",
    "settings",
    "probe",
    "auth",
    "users",
    "roles",
    "directory",
    "api-tokens",
    "audit",
    "alerts",
    "alert-rules",
    "silences",
    "channels",
    "other",
];

/// 路徑 → 路由分類（不含 agent_id 等變動的部分）
pub fn api_route(path: &str) -> usize {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let name = match segs.as_slice() {
        ["healthz"] | ["readyz"] | ["metrics"] => "probe",
        ["api", "v1", "hosts", _, "metrics", ..] => "metrics",
        ["api", "v1", "system", "health"] => "health",
        ["api", "v1", first, ..] => *first,
        _ => "other",
    };
    API_ROUTES
        .iter()
        .position(|r| *r == name)
        .unwrap_or(API_ROUTES.len() - 1)
}

/// HTTP 狀態碼的分類
pub const STATUS_CLASSES: [&str; 4] = ["2xx", "3xx", "4xx", "5xx"];

fn status_index(status: u16) -> usize {
    match status {
        200..=299 => 0,
        300..=399 => 1,
        400..=499 => 2,
        _ => 3,
    }
}

/// 主控台 API 的認證失敗（401、403、429）
pub const AUTH_FAILURES: [&str; 3] = ["unauthorized", "forbidden", "rate_limited"];

/// `/healthz`、`/readyz`、`/metrics` 的請求結果
pub const PROBE_RESULTS: [&str; 3] = ["ok", "not_ready", "denied"];

/// 所有計數器
pub struct Metrics {
    /// 離線（斷線）依原因
    pub disconnects: [Counter; 5],
    /// 上線（新上線、重新上線、離線後恢復）
    pub online: Counter,
    /// 同一個 agent_id 的新連線取代了舊連線
    pub superseded: Counter,
    /// 狀態表已滿，淘汰了離線最久的 agent
    pub registry_evictions: Counter,
    /// 狀態表已滿且沒有可淘汰者，拒絕新的 agent
    pub registry_full: Counter,
    /// report 速率超過上限：依憑證身分（mTLS）／依來源 IP
    pub report_rate_limited_identity: Counter,
    pub report_rate_limited_ip: Counter,
    /// 被拒的用戶端憑證，依原因
    pub cert_rejected: [Counter; 5],
    /// hello 的 agent_id 與用戶端憑證不符（`KRS-W1102`）
    pub agent_auth_failed: Counter,
    /// 事件匯流排的通知落後（計數可能少算）
    pub host_events_lagged: Counter,
    /// 告警事件建立（含彙總的子事件），依嚴重度
    pub alerts_fired: [Counter; 4],
    /// 告警事件解決，依嚴重度
    pub alerts_resolved: [Counter; 4],
    /// 告警的變化通知落後（計數可能少算）
    pub alerts_lagged: Counter,
    /// 通知的發送：[管道種類][結果]
    alert_sends: [[AtomicU64; 2]; 3],
    /// 通知的最後狀態：[管道種類][狀態]
    alert_deliveries: [[AtomicU64; 3]; 3],
    /// 主控台 API：[路由][狀態碼分類]
    api: [[AtomicU64; 4]; API_ROUTES.len()],
    pub api_auth_failures: [Counter; 3],
    /// `/healthz`、`/readyz`、`/metrics`：[端點][結果]
    probes: [[AtomicU64; 3]; 3],
}

impl Metrics {
    const fn new() -> Self {
        Self {
            disconnects: [const { Counter::new() }; 5],
            online: Counter::new(),
            superseded: Counter::new(),
            registry_evictions: Counter::new(),
            registry_full: Counter::new(),
            report_rate_limited_identity: Counter::new(),
            report_rate_limited_ip: Counter::new(),
            cert_rejected: [const { Counter::new() }; 5],
            agent_auth_failed: Counter::new(),
            host_events_lagged: Counter::new(),
            alerts_fired: [const { Counter::new() }; 4],
            alerts_resolved: [const { Counter::new() }; 4],
            alerts_lagged: Counter::new(),
            alert_sends: [const { [const { AtomicU64::new(0) }; 2] }; 3],
            alert_deliveries: [const { [const { AtomicU64::new(0) }; 3] }; 3],
            api: [const { [const { AtomicU64::new(0) }; 4] }; API_ROUTES.len()],
            api_auth_failures: [const { Counter::new() }; 3],
            probes: [const { [const { AtomicU64::new(0) }; 3] }; 3],
        }
    }

    pub fn disconnect(&self, r: DisconnectReason) {
        if let Some(i) = offline_index(r) {
            self.disconnects[i].inc();
        }
    }

    /// 匯流排的一則事件
    pub fn bus_event(&self, ev: &BusEvent) {
        match &ev.kind {
            EventKind::HostOnline { .. } => self.online.inc(),
            EventKind::HostOffline { reason, .. } => self.disconnect(*reason),
            EventKind::HostSuperseded { .. } => self.superseded.inc(),
            EventKind::AuthFailed { .. } => self.agent_auth_failed.inc(),
            EventKind::RegistryEvicted { .. } => self.registry_evictions.inc(),
            EventKind::HostDecommissioned { .. }
            | EventKind::HostRebooted { .. }
            | EventKind::TlsReload { .. }
            | EventKind::Condition { .. }
            | EventKind::Lifecycle { .. }
            | EventKind::ChannelSecrets { .. }
            | EventKind::AuditIntegrity { .. }
            | EventKind::RemoteCondition { .. }
            | EventKind::UpgradeWindow { .. } => {}
        }
    }

    /// 告警的一個變化（`alert.created`、`alert.updated`、`alert.resolved`）
    pub fn alert_change(&self, kind: &str, event: &serde_json::Value) {
        let Some(sev) = event
            .get("severity")
            .and_then(|v| v.as_str())
            .and_then(severity_index)
        else {
            return;
        };
        match kind {
            "alert.created" => self.alerts_fired[sev].inc(),
            "alert.resolved" => self.alerts_resolved[sev].inc(),
            _ => {}
        }
    }

    /// 一次發送的結果（`kind`：管道種類 `email`／`webhook`）
    pub fn alert_send(&self, kind: &str, ok: bool) {
        let k = channel_index(kind);
        self.alert_sends[k][usize::from(!ok)].fetch_add(1, Ordering::Relaxed);
    }

    /// 一則通知的最後狀態（[`DELIVERY_STATES`]；其他狀態不計）
    pub fn alert_delivery(&self, kind: &str, state: &str) {
        if let Some(s) = DELIVERY_STATES.iter().position(|x| *x == state) {
            self.alert_deliveries[channel_index(kind)][s].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 發送：(管道種類, 結果, 次數)
    pub fn alert_sends(&self) -> Vec<(&'static str, &'static str, u64)> {
        let mut out = Vec::new();
        for (k, row) in self.alert_sends.iter().enumerate() {
            for (r, n) in row.iter().enumerate() {
                out.push((CHANNEL_KINDS[k], SEND_RESULTS[r], n.load(Ordering::Relaxed)));
            }
        }
        out
    }

    /// 通知的最後狀態：(管道種類, 狀態, 則數)
    pub fn alert_deliveries(&self) -> Vec<(&'static str, &'static str, u64)> {
        let mut out = Vec::new();
        for (k, row) in self.alert_deliveries.iter().enumerate() {
            for (s, n) in row.iter().enumerate() {
                out.push((
                    CHANNEL_KINDS[k],
                    DELIVERY_STATES[s],
                    n.load(Ordering::Relaxed),
                ));
            }
        }
        out
    }

    pub fn cert_rejection(&self, r: RejectionReason) {
        self.cert_rejected[cert_index(r)].inc();
    }

    /// 主控台 API 的一個請求
    pub fn api_request(&self, path: &str, status: u16) {
        self.api[api_route(path)][status_index(status)].fetch_add(1, Ordering::Relaxed);
        match status {
            401 => self.api_auth_failures[0].inc(),
            403 => self.api_auth_failures[1].inc(),
            429 => self.api_auth_failures[2].inc(),
            _ => {}
        }
    }

    /// 主控台 API：(路由, 狀態碼分類, 次數)，只列出不為 0 的
    pub fn api_requests(&self) -> Vec<(&'static str, &'static str, u64)> {
        let mut out = Vec::new();
        for (r, row) in self.api.iter().enumerate() {
            for (c, n) in row.iter().enumerate() {
                let n = n.load(Ordering::Relaxed);
                if n > 0 {
                    out.push((API_ROUTES[r], STATUS_CLASSES[c], n));
                }
            }
        }
        out
    }

    /// 探針端點的一個請求（`endpoint`：0 healthz、1 readyz、2 metrics；`result`：[`PROBE_RESULTS`]）
    pub fn probe(&self, endpoint: usize, result: usize) {
        self.probes[endpoint.min(2)][result.min(2)].fetch_add(1, Ordering::Relaxed);
    }

    /// 探針端點：(端點, 結果, 次數)
    pub fn probe_requests(&self) -> Vec<(&'static str, &'static str, u64)> {
        let names = ["healthz", "readyz", "metrics"];
        let mut out = Vec::new();
        for (e, row) in self.probes.iter().enumerate() {
            for (r, n) in row.iter().enumerate() {
                out.push((names[e], PROBE_RESULTS[r], n.load(Ordering::Relaxed)));
            }
        }
        out
    }
}

static METRICS: Metrics = Metrics::new();

/// 行程內唯一的計數器
pub fn global() -> &'static Metrics {
    &METRICS
}

fn channel_index(kind: &str) -> usize {
    CHANNEL_KINDS[..2]
        .iter()
        .position(|k| *k == kind)
        .unwrap_or(2)
}

/// 訂閱內部事件匯流排，計數上線、各原因的離線、被取代、身分驗證失敗與狀態表淘汰（與告警引擎同一個
/// 來源；不必修改連線的程式）。關機時結束
pub async fn watch_bus(bus: Arc<EventBus>, shutdown: Arc<Shutdown>) {
    let mut rx = bus.subscribe();
    let m = global();
    loop {
        let ev = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            e = rx.recv() => e,
        };
        match ev {
            Ok(ev) => m.bus_event(&ev),
            Err(RecvError::Lagged(n)) => m.host_events_lagged.add(n),
            Err(RecvError::Closed) => return,
        }
    }
}

/// 訂閱告警的變化，計數建立與解決的事件（依嚴重度）。關機時結束
pub async fn watch_alerts(
    mut rx: tokio::sync::broadcast::Receiver<crate::alerts::engine::Change>,
    shutdown: Arc<Shutdown>,
) {
    let m = global();
    loop {
        let c = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            c = rx.recv() => c,
        };
        match c {
            Ok(c) => m.alert_change(c.kind, &c.event),
            Err(RecvError::Lagged(n)) => m.alerts_lagged.add(n),
            Err(RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bus_events_are_counted_once() {
        let m = Metrics::new();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let id: Arc<str> = Arc::from("a");
        bus.publish(EventKind::HostOnline {
            agent_id: id.clone(),
            hostname: id.clone(),
            ip: None,
        });
        bus.publish(EventKind::HostOffline {
            agent_id: id.clone(),
            hostname: id.clone(),
            ip: None,
            reason: DisconnectReason::WriteUnknown,
        });
        bus.publish(EventKind::HostSuperseded {
            agent_id: id.clone(),
            previous_ip: None,
            ip: None,
        });
        bus.publish(EventKind::RegistryEvicted {
            agent_id: "b".into(),
            hostname: "b".into(),
        });
        bus.publish(EventKind::AuthFailed {
            agent_id: "c".into(),
            ip: "192.0.2.1".parse().unwrap(),
        });
        for _ in 0..5 {
            m.bus_event(&rx.recv().await.unwrap());
        }
        assert_eq!(m.online.get(), 1);
        assert_eq!(m.disconnects[4].get(), 1);
        assert_eq!(m.superseded.get(), 1);
        assert_eq!(m.registry_evictions.get(), 1);
        assert_eq!(m.agent_auth_failed.get(), 1);
    }

    #[test]
    fn offline_reason_labels_match_the_bus_codes() {
        for r in [
            DisconnectReason::IdleTimeout,
            DisconnectReason::AgentClosed,
            DisconnectReason::ServerShutdown,
            DisconnectReason::CertificateRejected,
            DisconnectReason::WriteUnknown,
            DisconnectReason::ServerLost,
        ] {
            assert_eq!(OFFLINE_REASONS[offline_index(r).unwrap()], r.as_str());
        }
    }

    #[test]
    fn alert_counts_use_bounded_labels() {
        let m = Metrics::new();
        m.alert_change(
            "alert.created",
            &serde_json::json!({"severity": "critical"}),
        );
        m.alert_change(
            "alert.updated",
            &serde_json::json!({"severity": "critical"}),
        );
        m.alert_change(
            "alert.resolved",
            &serde_json::json!({"severity": "critical"}),
        );
        m.alert_change("alert.created", &serde_json::json!({"severity": "???"}));
        assert_eq!(m.alerts_fired[1].get(), 1);
        assert_eq!(m.alerts_resolved[1].get(), 1);
        assert_eq!(m.alerts_fired.iter().map(Counter::get).sum::<u64>(), 1);
        m.alert_send("webhook", true);
        m.alert_send("pagerduty", false);
        m.alert_delivery("email", "sent");
        m.alert_delivery("email", "pending");
        assert!(m.alert_sends().contains(&("webhook", "ok", 1)));
        assert!(m.alert_sends().contains(&("other", "error", 1)));
        assert!(m.alert_deliveries().contains(&("email", "sent", 1)));
        assert_eq!(m.alert_deliveries().iter().map(|x| x.2).sum::<u64>(), 1);
        assert_eq!(m.alert_sends().len(), 6);
    }

    #[test]
    fn routes_have_bounded_labels() {
        assert_eq!(API_ROUTES[api_route("/api/v1/hosts")], "hosts");
        assert_eq!(
            API_ROUTES[api_route("/api/v1/hosts/6f1c0e4a-1b2c-4d3e-9f00-112233445566")],
            "hosts"
        );
        assert_eq!(
            API_ROUTES[api_route("/api/v1/hosts/x/metrics/system.cpu.usage")],
            "metrics"
        );
        assert_eq!(API_ROUTES[api_route("/api/v1/system/health")], "health");
        assert_eq!(API_ROUTES[api_route("/api/v1/enroll/requests")], "enroll");
        assert_eq!(API_ROUTES[api_route("/metrics")], "probe");
        assert_eq!(API_ROUTES[api_route("/api/v1/unknown-thing")], "other");
        assert_eq!(API_ROUTES[api_route("/api/v1/alerts/E-1/ack")], "alerts");
        assert_eq!(
            API_ROUTES[api_route("/api/v1/channels/c-1/test")],
            "channels"
        );
        assert_eq!(API_ROUTES[api_route("/api/v1/auth/login")], "auth");
        assert_eq!(API_ROUTES[api_route("/api/v1/users/ann")], "users");
        assert_eq!(API_ROUTES[api_route("/favicon.ico")], "other");
        assert_eq!(STATUS_CLASSES[status_index(503)], "5xx");
    }

    #[test]
    fn api_counts_and_auth_failures() {
        let m = Metrics::new();
        m.api_request("/api/v1/hosts", 200);
        m.api_request("/api/v1/hosts", 200);
        m.api_request("/api/v1/me", 401);
        m.api_request("/api/v1/me", 429);
        assert!(m.api_requests().contains(&("hosts", "2xx", 2)));
        assert!(m.api_requests().contains(&("me", "4xx", 2)));
        assert_eq!(m.api_auth_failures[0].get(), 1);
        assert_eq!(m.api_auth_failures[2].get(), 1);
        m.disconnect(DisconnectReason::IdleTimeout);
        assert_eq!(m.disconnects[0].get(), 1);
        m.disconnect(DisconnectReason::Superseded);
        assert_eq!(m.disconnects.iter().map(Counter::get).sum::<u64>(), 1);
        m.probe(2, 2);
        assert!(m.probe_requests().contains(&("metrics", "denied", 1)));
    }
}
