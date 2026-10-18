//! 本機的 Prometheus 指標（審計 O3）：連線狀態、送出／ack／補送佇列、取樣耗時與逾時。
//!
//! 預設關閉。兩種提供方式（可同時使用，見 `[metrics]`）：
//!
//! - `metrics.listen`：本機 HTTP `GET /metrics`（只接受 loopback 位址，不需要認證；一次一個請求、
//!   回應後關閉連線）。
//! - `metrics.textfile`：node_exporter textfile collector 的檔案（`*.prom`），每 `textfile_interval`
//!   秒以「寫暫存檔再改名」的方式更新；Pulse 正常結束時刪除（避免留下過時的數值）。
//!
//! 計數器由各任務經 [`crate::local_status`] 的狀態更新一併記錄（不另外插入連線的程式）；輸出時讀取
//! 本機狀態（與 `status.json` 同一份資料）與這裡的累計值。標籤只用有限集合的值（狀態、原因、錯誤代碼），
//! 不以 agent_id、主機名稱或位址當標籤。

use chrono::{DateTime, Utc};
use kp_obs::prom::{Counter, Exposition, Histogram, Kind};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::config_manager::MetricsConfig;
use crate::status_file::{self as sf, StatusFile};

/// 系統資訊取樣耗時的桶（秒）
pub const SAMPLE_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// 在途（已送出、尚未收到回應）的 report 最多記錄這麼多筆的送出時間
const MAX_INFLIGHT: usize = 4096;

/// 連線狀態（`status.json` 的 `connection.state`）
pub const CONNECTION_STATES: [&str; 4] = ["connecting", "registered", "reconnecting", "rejected"];

/// 累計值
pub struct Metrics {
    pub started: DateTime<Utc>,
    pub sampled: Counter,
    pub sent: Counter,
    pub acked: Counter,
    pub rejected: Counter,
    pub dropped_disconnected: Counter,
    pub registrations: Counter,
    /// 同一輪中改試下一個候選位址的次數（`server.fallback_addresses`）
    pub failovers: Counter,
    /// 收到 Kairos 的排空（`drain`）而改連的次數
    pub drains: Counter,
    /// 候選位址的數量與目前（最後）連上的是第幾個
    candidate: Mutex<(usize, usize)>,
    /// 斷線（連線失敗）依錯誤代碼（無法歸類為 `unknown`）
    disconnects: Mutex<BTreeMap<String, u64>>,
    pub sample_duration: Histogram,
    pub sample_timeouts: Counter,
    /// 送出 report 到收到回應（ack 或拒收）的時間
    pub ack_latency: Histogram,
    inflight: Mutex<VecDeque<Instant>>,
}

static METRICS: LazyLock<Metrics> = LazyLock::new(|| Metrics {
    started: Utc::now(),
    sampled: Counter::new(),
    sent: Counter::new(),
    acked: Counter::new(),
    rejected: Counter::new(),
    dropped_disconnected: Counter::new(),
    registrations: Counter::new(),
    failovers: Counter::new(),
    drains: Counter::new(),
    candidate: Mutex::new((1, 0)),
    disconnects: Mutex::new(BTreeMap::new()),
    sample_duration: Histogram::new(SAMPLE_BUCKETS),
    sample_timeouts: Counter::new(),
    ack_latency: Histogram::latency(),
    inflight: Mutex::new(VecDeque::new()),
});

/// 行程內唯一的累計值
pub fn global() -> &'static Metrics {
    &METRICS
}

impl Metrics {
    /// 送出一筆 report
    pub fn report_sent(&self) {
        self.sent.inc();
        let mut q = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= MAX_INFLIGHT {
            q.pop_front();
        }
        q.push_back(Instant::now());
    }

    /// 收到一筆 report 的回應（伺服器依序回應：對應最早送出的那一筆）
    pub fn report_answered(&self, acked: bool) {
        if acked {
            self.acked.inc();
        } else {
            self.rejected.inc();
        }
        let sent = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        if let Some(t) = sent {
            self.ack_latency.observe(t.elapsed());
        }
    }

    /// 連線結束或重新註冊：在途的不會再有回應
    pub fn connection_reset(&self) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 候選位址的數量與目前連上的是第幾個（從 0 起）
    pub fn set_candidate(&self, count: usize, index: usize) {
        *self.candidate.lock().unwrap_or_else(|e| e.into_inner()) = (count, index);
    }

    /// 只設定候選位址的數量（啟動時）
    pub fn set_candidate_count(&self, count: usize) {
        self.candidate.lock().unwrap_or_else(|e| e.into_inner()).0 = count;
    }

    /// 一次斷線（連線失敗）
    pub fn disconnected(&self, code: Option<&str>) {
        self.connection_reset();
        *self
            .disconnects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(code.unwrap_or("unknown").to_string())
            .or_default() += 1;
    }

    fn disconnects(&self) -> Vec<(String, u64)> {
        self.disconnects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
}

fn b(v: bool) -> f64 {
    if v { 1.0 } else { 0.0 }
}

fn unix(ts: Option<&str>) -> Option<f64> {
    ts.and_then(sf::parse_ts)
        .map(|t| t.timestamp_millis() as f64 / 1000.0)
}

/// 產生 `/metrics` 的內容（`status`：本機狀態的目前內容；未初始化時為 `None`）
pub fn render(status: Option<&StatusFile>) -> String {
    let m = global();
    let mut e = Exposition::new();
    e.family(
        "pulse_build_info",
        Kind::Gauge,
        "Pulse 的版本與建置的 commit（值固定為 1）",
    )
    .sample(
        "pulse_build_info",
        &[
            ("version", env!("CARGO_PKG_VERSION")),
            ("commit", env!("PULSE_BUILD_COMMIT")),
        ],
        1.0,
    );
    e.gauge(
        "pulse_start_time_seconds",
        "啟動時間（Unix 秒）",
        m.started.timestamp() as f64,
    );

    // ---- 連線 ----
    let state = status.map_or("connecting", |s| s.connection.state.as_str());
    e.gauge(
        "pulse_connected",
        "是否已註冊到 Kairos（1 是）",
        b(state == "registered"),
    );
    e.gauge_by(
        "pulse_connection_state",
        "連線狀態（目前的狀態為 1；rejected：需要人工處理的錯誤，例如憑證被撤銷）",
        "state",
        &CONNECTION_STATES.map(|s| (s, b(s == state))),
    );
    e.gauge(
        "pulse_connection_consecutive_failures",
        "目前連續失敗的次數",
        status.map_or(0, |s| s.connection.consecutive_failures) as f64,
    );
    e.counter(
        "pulse_registrations_total",
        "收到 hello_ack、完成註冊的次數",
        m.registrations.get(),
    );
    let (candidates, candidate_index) = *m.candidate.lock().unwrap_or_else(|e| e.into_inner());
    e.gauge(
        "pulse_server_candidates",
        "伺服器候選位址的數量（server.address 加 server.fallback_addresses）",
        candidates as f64,
    );
    e.gauge(
        "pulse_server_candidate_index",
        "目前（或最後一次）連上的是第幾個候選位址（從 0 起；0 為 server.address）",
        candidate_index as f64,
    );
    e.counter(
        "pulse_connect_failovers_total",
        "同一輪中某個候選位址無法連線、改試下一個的次數",
        m.failovers.get(),
    );
    e.counter(
        "pulse_drains_total",
        "收到 Kairos 的排空（drain）而改連其他節點的次數",
        m.drains.get(),
    );
    let disconnects = m.disconnects();
    let d: Vec<(&str, u64)> = disconnects.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    e.counter_by(
        "pulse_disconnects_total",
        "連線中斷或連線失敗的次數，依錯誤代碼（無法歸類為 unknown）",
        "code",
        &d,
    );

    // ---- report ----
    e.counter(
        "pulse_reports_sampled_total",
        "系統資訊取樣的次數",
        m.sampled.get(),
    );
    e.counter(
        "pulse_reports_sent_total",
        "送出的 report 數（含補送）",
        m.sent.get(),
    );
    e.counter(
        "pulse_reports_acked_total",
        "Kairos 確認（ack）的 report 數",
        m.acked.get(),
    );
    e.counter(
        "pulse_reports_rejected_total",
        "Kairos 拒收或無法保存的 report 數",
        m.rejected.get(),
    );
    e.counter_by(
        "pulse_reports_dropped_total",
        "沒有送出就丟棄的取樣，依原因（disconnected：不補送時未連線、寫出中或上一筆未回應）",
        "reason",
        &[("disconnected", m.dropped_disconnected.get())],
    );
    e.histogram(
        "pulse_report_ack_duration_seconds",
        "送出 report 到收到 Kairos 回應的時間",
        &m.ack_latency.snapshot(),
    );
    e.histogram(
        "pulse_sample_duration_seconds",
        "一次系統資訊取樣的耗時",
        &m.sample_duration.snapshot(),
    );
    e.counter(
        "pulse_sample_timeouts_total",
        "系統資訊取樣逾時、略過 report 的次數（PLS-W8005）",
        m.sample_timeouts.get(),
    );

    // ---- 本機緩衝（補送佇列） ----
    if let Some(s) = status {
        let r = &s.reporting;
        let buf = &r.buffer;
        e.gauge(
            "pulse_delivery_durable",
            "是否以 at-least-once（本機緩衝＋補送）投遞（1 是）",
            b(r.delivery == "durable"),
        );
        e.gauge(
            "pulse_buffer_enabled",
            "是否啟用本機緩衝（1 是）",
            b(buf.enabled),
        );
        if buf.enabled {
            e.gauge(
                "pulse_buffer_available",
                "本機緩衝是否可用（1 是；0 表示無法開啟，PLS-E4004）",
                b(buf.state == "ok"),
            );
            e.gauge(
                "pulse_buffer_pending_records",
                "待送（補送佇列中）的筆數",
                buf.pending.unwrap_or(0) as f64,
            );
            e.gauge(
                "pulse_buffer_pending_bytes",
                "待送資料的大小",
                buf.pending_bytes.unwrap_or(0) as f64,
            );
            e.gauge(
                "pulse_buffer_max_bytes",
                "本機緩衝的大小上限（buffer.max_mb）",
                buf.max_bytes.unwrap_or(0) as f64,
            );
            e.gauge(
                "pulse_buffer_max_records",
                "本機緩衝的筆數上限（buffer.max_records）",
                buf.max_records.unwrap_or(0) as f64,
            );
            e.gauge(
                "pulse_buffer_usage_ratio",
                "本機緩衝的使用率（大小與筆數中較高者）",
                buf.usage_ratio.unwrap_or(0.0),
            );
            if let Some(t) = unix(buf.oldest_sample_at.as_deref()) {
                e.gauge(
                    "pulse_buffer_oldest_sample_timestamp_seconds",
                    "最舊一筆待送資料的取樣時間（Unix 秒）",
                    t,
                );
            }
            e.gauge(
                "pulse_buffer_replaying",
                "是否正在補送（1 是）",
                b(buf.replaying == Some(true)),
            );
            e.counter_by(
                "pulse_buffer_dropped_total",
                "從本機緩衝丟棄的筆數，依原因（full：緩衝已滿 PLS-W4202；expired：超過保存期限 PLS-W4203；rejected：被 Kairos 拒收）",
                "reason",
                &[
                    ("full", buf.dropped_full.unwrap_or(0)),
                    ("expired", buf.dropped_expired.unwrap_or(0)),
                    ("rejected", buf.dropped_rejected.unwrap_or(0)),
                ],
            );
            e.counter(
                "pulse_buffer_write_failures_total",
                "寫入本機緩衝失敗的次數（PLS-W4103）",
                buf.write_failures.unwrap_or(0),
            );
        }
        if let Some(ms) = s.clock.offset_ms {
            e.gauge(
                "pulse_clock_offset_seconds",
                "本機時間減 Kairos 時間（正值＝本機較快；最近一次量測）",
                ms as f64 / 1000.0,
            );
        }
        if let Some(t) = unix(s.identity.not_after.as_deref()) {
            e.gauge(
                "pulse_client_cert_expiry_timestamp_seconds",
                "用戶端憑證的到期時間（Unix 秒）",
                t,
            );
        }
    }

    // ---- 日誌 ----
    e.counter_by(
        "pulse_log_messages_total",
        "記錄的日誌行數，依等級",
        "level",
        &kp_obs::logfmt::line_counts(),
    );
    if let Some(s) = crate::log_files::sink().and_then(|s| s.syslog.as_ref()) {
        use std::sync::atomic::Ordering;
        e.counter_by(
            "pulse_log_syslog_messages_total",
            "送往 syslog 的日誌行，依結果（dropped：佇列已滿或接收端無法連線）",
            "result",
            &[
                ("sent", s.stats.sent.load(Ordering::Relaxed)),
                ("dropped", s.stats.dropped.load(Ordering::Relaxed)),
            ],
        );
    }
    e.finish()
}

/// 目前的內容（本機狀態不清除「有變化」的標記，不影響狀態檔的寫出）
pub fn current() -> String {
    let status = crate::local_status::tracker().map(|t| t.peek());
    render(status.as_ref())
}

/// 依設定啟動 HTTP 與 textfile（都沒有設定時什麼都不做）
pub fn spawn(config: &MetricsConfig, base: &std::path::Path, shutdown: CancellationToken) {
    match config.listen_addr() {
        Ok(Some(addr)) => {
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => {
                        log::info!("本機指標：http://{addr}/metrics（只接受本機連線）");
                        serve(l, shutdown).await;
                    }
                    Err(e) => log::warn!(
                        "本機指標無法監聽 metrics.listen {addr}：{e}；監控與回報照常，只是沒有 /metrics"
                    ),
                }
            });
        }
        Ok(None) => {}
        Err(e) => log::warn!("{e}"),
    }
    let textfile = config.textfile.trim();
    if !textfile.is_empty() {
        let path = base.join(textfile);
        let every = Duration::from_secs(config.textfile_interval.max(5));
        log::info!(
            "本機指標：每 {} 秒寫入 {}（node_exporter textfile collector）",
            every.as_secs(),
            path.display()
        );
        tokio::spawn(write_textfile_periodically(path, every, shutdown));
    }
}

/// 一次讀取請求的上限
const MAX_REQUEST_BYTES: usize = 8 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

async fn serve(listener: tokio::net::TcpListener, shutdown: CancellationToken) {
    loop {
        let (mut socket, _) = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        // 一次一個請求、處理完才接受下一個（本機的抓取，數量很少；避免被灌爆時佔用資源）
        let _ = tokio::time::timeout(REQUEST_TIMEOUT, async {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < MAX_REQUEST_BYTES {
                let n = socket.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let resp = response(&String::from_utf8_lossy(&buf));
            socket.write_all(&resp).await?;
            socket.shutdown().await
        })
        .await;
    }
}

/// 依請求行產生回應（GET／HEAD /metrics，其餘 404 或 405）
pub fn response(request: &str) -> Vec<u8> {
    let line = request.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = path.split('?').next().unwrap_or("");
    let (status, ctype, body) = match (method, path) {
        ("GET" | "HEAD", "/metrics") => ("200 OK", kp_obs::prom::CONTENT_TYPE, current()),
        (_, "/metrics") => (
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "method not allowed\n".to_string(),
        ),
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found\n".to_string(),
        ),
    };
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if method != "HEAD" {
        out.extend_from_slice(body.as_bytes());
    }
    out
}

/// 原子地寫入 textfile（同目錄的暫存檔再改名；node_exporter 不會讀到寫一半的檔案）
pub fn write_textfile(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // 暫存檔不以 .prom 結尾，node_exporter 不會讀到
    let tmp: PathBuf = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644));
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

async fn write_textfile_periodically(path: PathBuf, every: Duration, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failing = false;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                // 正常結束時刪除，避免 node_exporter 繼續提供過時的數值
                let _ = std::fs::remove_file(&path);
                return;
            }
            _ = ticker.tick() => {}
        }
        let text = current();
        let p = path.clone();
        let result = tokio::task::spawn_blocking(move || write_textfile(&p, &text))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
        match result {
            Ok(()) if failing => {
                failing = false;
                log::info!("本機指標的 textfile 已恢復寫入（{}）", path.display());
            }
            Ok(()) => {}
            Err(e) if !failing => {
                failing = true;
                log::warn!(
                    "無法寫入本機指標的 textfile {}：{e}；監控與回報照常，恢復前不再重複記錄",
                    path.display()
                );
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_valid_prometheus_text() {
        let m = global();
        m.sampled.inc();
        m.report_sent();
        m.report_answered(true);
        m.report_sent();
        m.report_answered(false);
        m.disconnected(Some("PLS-E2003"));
        m.disconnected(None);
        m.sample_duration.observe(Duration::from_millis(30));
        let mut status = StatusFile::default();
        status.connection.state = "registered".into();
        status.reporting.delivery = "durable".into();
        status.reporting.buffer.enabled = true;
        status.reporting.buffer.state = "ok".into();
        status.reporting.buffer.pending = Some(12);
        status.reporting.buffer.oldest_sample_at = Some("2026-10-14T06:00:00.000Z".into());
        status.clock.offset_ms = Some(-1500);
        let text = render(Some(&status));
        let p = kp_obs::prom::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        kp_obs::prom::lint(&p, "pulse_").unwrap();
        assert_eq!(p.get("pulse_connected", &[]), Some(1.0));
        assert_eq!(
            p.get("pulse_connection_state", &[("state", "registered")]),
            Some(1.0)
        );
        assert_eq!(p.get("pulse_buffer_pending_records", &[]), Some(12.0));
        assert_eq!(p.get("pulse_clock_offset_seconds", &[]), Some(-1.5));
        assert!(
            p.get("pulse_disconnects_total", &[("code", "PLS-E2003")])
                .unwrap()
                >= 1.0
        );
        assert!(
            p.get("pulse_disconnects_total", &[("code", "unknown")])
                .unwrap()
                >= 1.0
        );
        assert!(
            p.get("pulse_report_ack_duration_seconds_count", &[])
                .unwrap()
                >= 2.0
        );
        assert!(p.get("pulse_sample_duration_seconds_count", &[]).unwrap() >= 1.0);
        assert_eq!(
            p.get("pulse_buffer_oldest_sample_timestamp_seconds", &[]),
            Some(1_791_957_600.0)
        );
        // 沒有本機狀態時也合法
        let p = kp_obs::prom::parse(&render(None)).unwrap();
        kp_obs::prom::lint(&p, "pulse_").unwrap();
        assert_eq!(p.get("pulse_connected", &[]), Some(0.0));
    }

    #[test]
    fn http_responses() {
        let r = String::from_utf8(response("GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")).unwrap();
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
        assert!(r.contains("pulse_build_info"));
        let r = String::from_utf8(response("HEAD /metrics HTTP/1.1\r\n\r\n")).unwrap();
        assert!(r.ends_with("\r\n\r\n"), "HEAD 沒有本文");
        let r = String::from_utf8(response("POST /metrics HTTP/1.1\r\n\r\n")).unwrap();
        assert!(r.starts_with("HTTP/1.1 405"));
        let r = String::from_utf8(response("GET / HTTP/1.1\r\n\r\n")).unwrap();
        assert!(r.starts_with("HTTP/1.1 404"));
        let r = String::from_utf8(response("")).unwrap();
        assert!(r.starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn textfile_is_replaced_atomically() {
        let dir = std::env::temp_dir().join(format!("pulse-metrics-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pulse.prom");
        write_textfile(&path, "a 1\n").unwrap();
        write_textfile(&path, "a 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a 2\n");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "不應留下暫存檔：{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn http_listener_serves_metrics() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve(l, stop.clone()));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        let body = text.split_once("\r\n\r\n").unwrap().1;
        kp_obs::prom::parse(body).unwrap();
        stop.cancel();
        task.await.unwrap();
    }
}
