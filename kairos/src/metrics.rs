//! 執行中的計數器（審計 O3；設計包 `kairos/screens.md` S20、§24 #8）：`/metrics` 與 `/readyz` 讀取。
//!
//! 只有單純的原子計數器與直方圖（[`kp_obs::prom`]），由各處以一行呼叫記錄；計數不經過鎖，
//! 不影響 report 的 `ack` 延遲。已在其他模組計數的項目（report 寫入結果 [`crate::stats::WriteStats`]、
//! 被拒的連線 `per_ip::stats`、註冊端點 `enroll::EnrollStats`）不重複，在輸出時直接讀取。
//!
//! 標籤只用有限集合的值（原因、結果、路由的分類）；不以 agent_id、IP 或路徑當標籤。
//!
//! 另一條工作線（內部事件匯流排）完成後，可由事件訂閱改為在同一處計數；這裡的介面不變。

use kp_obs::prom::Counter;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast::error::RecvError;

use crate::registry::{HostChangeKind, OfflineReason, Registry};
use crate::shutdown::Shutdown;
use crate::tls::RejectionReason;

/// 斷線（離線）原因（[`OfflineReason`]）的標籤值，順序與 [`offline_index`] 相同
pub const OFFLINE_REASONS: [&str; 4] = [
    "timeout",
    "connection_closed",
    "server_shutdown",
    "certificate_rejected",
];

fn offline_index(r: OfflineReason) -> usize {
    match r {
        OfflineReason::Timeout => 0,
        OfflineReason::ConnectionClosed => 1,
        OfflineReason::ServerShutdown => 2,
        OfflineReason::CertificateRejected => 3,
    }
}

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
pub const API_ROUTES: [&str; 11] = [
    "me", "hosts", "metrics", "overview", "health", "events", "enroll", "pki", "settings", "probe",
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
    pub disconnects: [Counter; 4],
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
    /// 狀態表的變化通知落後（計數可能少算）
    pub host_events_lagged: Counter,
    /// 主控台 API：[路由][狀態碼分類]
    api: [[AtomicU64; 4]; 11],
    pub api_auth_failures: [Counter; 3],
    /// `/healthz`、`/readyz`、`/metrics`：[端點][結果]
    probes: [[AtomicU64; 3]; 3],
}

impl Metrics {
    const fn new() -> Self {
        Self {
            disconnects: [const { Counter::new() }; 4],
            online: Counter::new(),
            superseded: Counter::new(),
            registry_evictions: Counter::new(),
            registry_full: Counter::new(),
            report_rate_limited_identity: Counter::new(),
            report_rate_limited_ip: Counter::new(),
            cert_rejected: [const { Counter::new() }; 5],
            host_events_lagged: Counter::new(),
            api: [const { [const { AtomicU64::new(0) }; 4] }; 11],
            api_auth_failures: [const { Counter::new() }; 3],
            probes: [const { [const { AtomicU64::new(0) }; 3] }; 3],
        }
    }

    pub fn disconnect(&self, r: OfflineReason) {
        self.disconnects[offline_index(r)].inc();
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

/// 訂閱狀態表的變化，計數上線與各原因的離線（不必修改連線的程式）。關機時結束
pub async fn watch_host_changes(registry: Arc<Registry>, shutdown: Arc<Shutdown>) {
    let mut rx = registry.subscribe();
    let m = global();
    loop {
        let change = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            c = rx.recv() => c,
        };
        match change {
            Ok(c) => match c.kind {
                HostChangeKind::Online => m.online.inc(),
                HostChangeKind::Offline(r) => m.disconnect(r),
            },
            Err(RecvError::Lagged(n)) => m.host_events_lagged.add(n),
            Err(RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        m.disconnect(OfflineReason::Timeout);
        assert_eq!(m.disconnects[0].get(), 1);
        m.probe(2, 2);
        assert!(m.probe_requests().contains(&("metrics", "denied", 1)));
    }
}
