//! `/healthz`、`/readyz`、`/metrics`（審計 O3；設計包 `kairos/screens.md` S20、§24 #8）。
//!
//! | 端點 | 認證 | 內容 |
//! |---|---|---|
//! | `/healthz` | 不需要 | 存活：行程在服務請求就回 200 `{"status":"ok"}`；關機中 503 |
//! | `/readyz` | 不需要 | 就緒：agent 連線埠在監聽、資料庫可寫、儲存不是 `storage_unavailable`、不在關機中；不符時 503。只回傳各項的狀態字，不含版本、路徑、數量與位址 |
//! | `/metrics` | `observability.metrics_token_file` 設定時必須帶 `Authorization: Bearer <token>`；未設定時只接受 loopback 來源 | Prometheus 文字格式 |
//!
//! 提供的位置：主控台的監聽（`console.listen`，HTTPS；`observability.on_console = false` 時不提供），
//! 以及可選的純 HTTP 監聽 `observability.listen`（只有這三個端點，在自己的執行緒與 runtime 上執行，
//! 與 agent 連線、report 的寫入分開排程）。`/readyz?scope=enroll` 另外要求註冊端點（8444）在應該
//! 監聽時有在監聽，給只負責 8444 的負載平衡器使用。
//!
//! 計數與輸出的成本：計數都是原子操作；資料庫的檢查（可寫、用量、保留清除的狀態）最多每
//! [`DB_CACHE`] 做一次，在 blocking 執行緒上進行。

use chrono::{DateTime, Utc};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use kp_obs::prom::{Exposition, Kind};
use serde_json::{Value, json};
use std::io::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::api::http::ApiBody;
use crate::config::AppConfig;
use crate::expiry::ExpiryKind;
use crate::metrics::{self, CERT_REJECTION_REASONS, OFFLINE_REASONS};
use crate::per_ip::PerIpLimiter;
use crate::registry::Registry;
use crate::shutdown::Shutdown;
use crate::stats::WriteStats;
use crate::storage::QueueProbe;
use crate::tls::TlsState;

/// 資料庫檢查結果的快取時間
pub const DB_CACHE: Duration = Duration::from_secs(5);
/// 資料庫檢查等待鎖的上限
pub const DB_BUSY_TIMEOUT: Duration = Duration::from_millis(500);
/// 純 HTTP 監聽的同時連線上限
pub const MAX_PROBE_CONNECTIONS: usize = 64;

/// 端點需要的狀態（啟動時建立，主控台與純 HTTP 監聽共用）
pub struct ObsContext {
    pub config: Arc<AppConfig>,
    pub registry: Arc<Registry>,
    pub write_stats: Arc<WriteStats>,
    pub queue: QueueProbe,
    pub per_ip: Arc<PerIpLimiter>,
    pub tls: Arc<TlsState>,
    pub enroll: Option<Arc<crate::enroll::EnrollService>>,
    pub shutdown: Arc<Shutdown>,
    pub started: DateTime<Utc>,
    /// agent 連線埠的 accept 迴圈已開始
    pub agent_listening: AtomicBool,
    /// SQLite 時的資料庫檢查（JSONL 時為 `None`）
    pub db: Option<DbProbe>,
    /// `/metrics` 的 bearer token
    pub metrics_token: Option<String>,
    /// 告警服務（啟動時；`/metrics` 的告警引擎狀態）
    pub alerts: Option<Arc<crate::alerts::AlertService>>,
}

/// [`ObsContext::new`] 的參數
pub struct ObsParams {
    pub config: Arc<AppConfig>,
    pub registry: Arc<Registry>,
    pub reports: crate::storage::ReportSink,
    pub per_ip: Arc<PerIpLimiter>,
    pub tls: Arc<TlsState>,
    pub enroll: Option<Arc<crate::enroll::EnrollService>>,
    pub shutdown: Arc<Shutdown>,
    /// SQLite 的資料庫檔（JSONL 時為 `None`）
    pub db_path: Option<PathBuf>,
    /// 告警服務（`alerts.enabled` 且已啟動時）
    pub alerts: Option<Arc<crate::alerts::AlertService>>,
}

impl ObsContext {
    /// 建立（讀取 `/metrics` 的 token 檔；無法讀取時為設定錯誤）
    pub fn new(p: ObsParams) -> Result<Self, Error> {
        let metrics_token = p.config.observability.metrics_token()?;
        Ok(Self {
            write_stats: p.reports.stats().clone(),
            queue: p.reports.queue_probe(),
            config: p.config,
            registry: p.registry,
            per_ip: p.per_ip,
            tls: p.tls,
            enroll: p.enroll,
            shutdown: p.shutdown,
            started: Utc::now(),
            agent_listening: AtomicBool::new(false),
            db: p.db_path.map(DbProbe::new),
            metrics_token,
            alerts: p.alerts,
        })
    }

    /// `/metrics` 是否允許這個請求
    pub fn metrics_allowed(&self, peer: SocketAddr, authorization: Option<&str>) -> bool {
        match &self.metrics_token {
            Some(token) => authorization
                .and_then(|v| v.strip_prefix("Bearer "))
                .is_some_and(|given| {
                    use subtle::ConstantTimeEq;
                    given.trim().len() == token.len()
                        && bool::from(given.trim().as_bytes().ct_eq(token.as_bytes()))
                }),
            None => crate::config::is_loopback(peer.ip()),
        }
    }
}

// ---- 資料庫檢查 -------------------------------------------------------------------------------

/// 一次資料庫檢查的結果
#[derive(Debug, Clone)]
pub struct DbSnapshot {
    /// 可寫（可以讀寫模式開啟；不取得寫入鎖）；失敗時為原因
    pub writable: Result<(), String>,
    pub used_bytes: Option<u64>,
    pub file_bytes: u64,
    pub wal_bytes: u64,
    /// 已達容量上限、正在提早刪除最舊的資料
    pub capacity_deleting: bool,
    /// 系統日期異常，暫停保留清除
    pub retention_paused: bool,
}

impl Default for DbSnapshot {
    fn default() -> Self {
        Self {
            writable: Ok(()),
            used_bytes: None,
            file_bytes: 0,
            wal_bytes: 0,
            capacity_deleting: false,
            retention_paused: false,
        }
    }
}

/// SQLite 的檢查：每次開一條新的連線（資料庫被替換時不會檢查到舊的檔案），結果快取 [`DB_CACHE`]
pub struct DbProbe {
    path: PathBuf,
    cached: Mutex<Option<(Instant, DbSnapshot)>>,
}

impl DbProbe {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cached: Mutex::new(None),
        }
    }

    fn check_blocking(&self) -> DbSnapshot {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, snap)) = cached.as_ref()
            && at.elapsed() < DB_CACHE
        {
            return snap.clone();
        }
        let snap = self.probe_now();
        *cached = Some((Instant::now(), snap.clone()));
        snap
    }

    fn probe_now(&self) -> DbSnapshot {
        use rusqlite::{Connection, OpenFlags};
        let size = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len());
        let mut wal = self.path.as_os_str().to_os_string();
        wal.push("-wal");
        let mut snap = DbSnapshot {
            file_bytes: size(&self.path),
            wal_bytes: size(std::path::Path::new(&wal)),
            ..DbSnapshot::default()
        };
        let conn = match Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(c) => c,
            Err(e) => {
                snap.writable = Err(format!("無法開啟資料庫：{e}"));
                return snap;
            }
        };
        let _ = conn.busy_timeout(DB_BUSY_TIMEOUT);
        // 不取得寫入鎖（BEGIN IMMEDIATE 會與 writer 搶鎖，writer 的忙碌等待以毫秒為單位，量測到 ack 的 p99 上升）：
        // 讀一次 schema（WAL 的讀者不擋 writer）後確認連線不是唯讀開啟（檔案或目錄沒有寫入權限、
        // 唯讀掛載時 SQLite 會退為唯讀）。實際的寫入失敗另由 /readyz 的 storage 反映
        snap.writable = conn
            .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
            .map_err(|e| format!("無法讀取資料庫：{e}"))
            .and_then(|_| match conn.is_readonly(rusqlite::MAIN_DB) {
                Ok(false) => Ok(()),
                Ok(true) => Err("資料庫只能以唯讀開啟（權限或唯讀掛載）".to_string()),
                Err(e) => Err(format!("無法確認資料庫是否可寫：{e}")),
            });
        if let Ok(stats) = crate::api::health::read_db_stats(&conn) {
            snap.used_bytes = Some(stats.used_bytes);
            snap.capacity_deleting = stats
                .capacity
                .as_ref()
                .is_some_and(|c| c.ended_at.is_none());
            snap.retention_paused = stats
                .guard
                .as_ref()
                .is_some_and(|g| g.jump_since_unix.is_some() || g.rollback_since_unix.is_some());
        }
        snap
    }
}

// ---- 就緒 -------------------------------------------------------------------------------------

/// 最近 1 分鐘的 report 都沒有保存成功、且有 `storage_unavailable` 或結果不明：儲存無法使用
pub fn storage_unavailable(stats: &WriteStats) -> bool {
    let w = stats.window(Instant::now(), 1);
    w.storage_unavailable + w.unknown > 0 && w.written + w.duplicate == 0
}

/// 註冊端點的狀態字（`/readyz` 與 `/metrics`）
fn enroll_state(ctx: &ObsContext) -> &'static str {
    let Some(svc) = &ctx.enroll else {
        return "disabled";
    };
    let state = svc.mode_state();
    let listening = svc
        .listen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .listening;
    match (state, listening) {
        (crate::enroll::ModeState::Closed, _) => "closed",
        (_, true) => "ok",
        (_, false) => "not_listening",
    }
}

/// `/readyz` 的結果：(是否就緒, 本文)
pub async fn readiness(
    ctx: &Arc<ObsContext>,
    db: Option<&DbSnapshot>,
    enroll_required: bool,
) -> (bool, Value) {
    let shutting_down = ctx.shutdown.started_at().is_some();
    let listening = ctx.agent_listening.load(Ordering::Relaxed);
    let database = match db {
        None => "not_applicable",
        Some(d) if d.writable.is_ok() => "ok",
        Some(_) => "unwritable",
    };
    let storage = if storage_unavailable(&ctx.write_stats) {
        "unavailable"
    } else {
        "ok"
    };
    let enroll = enroll_state(ctx);
    let mut ready = !shutting_down && listening && database != "unwritable" && storage == "ok";
    if enroll_required {
        ready &= enroll == "ok";
    }
    let body = json!({
        "status": if ready { "ready" } else { "not_ready" },
        "checks": {
            "shutdown": if shutting_down { "shutting_down" } else { "ok" },
            "agent_listener": if listening { "ok" } else { "not_listening" },
            "database": database,
            "storage": storage,
            "enroll": enroll,
        },
    });
    (ready, body)
}

// ---- /metrics ---------------------------------------------------------------------------------

fn b(v: bool) -> f64 {
    if v { 1.0 } else { 0.0 }
}

/// Linux 的行程資訊：(常駐記憶體 bytes, 開啟的檔案描述元數, CPU 秒數)
fn process_stats() -> Option<(u64, u64, f64)> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let fds = std::fs::read_dir("/proc/self/fd").ok()?.count() as u64;
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // comm 可能含空白：從最後一個 ')' 之後算欄位（utime、stime 為第 14、15 欄）
        let rest = &stat[stat.rfind(')')? + 2..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ticks: f64 = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
        // 每秒 100 個 tick（Linux 的 USER_HZ 實際上固定為 100）
        Some((rss_pages * 4096, fds, ticks / 100.0))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 產生 `/metrics` 的內容
pub async fn render(ctx: &Arc<ObsContext>) -> String {
    let db = ctx_db(ctx).await;
    render_with(ctx, db.as_ref()).await
}

/// 取得資料庫檢查（快取）
pub async fn ctx_db(ctx: &Arc<ObsContext>) -> Option<DbSnapshot> {
    ctx.db.as_ref()?;
    let ctx = ctx.clone();
    Some(
        tokio::task::spawn_blocking(move || ctx.db.as_ref().map(DbProbe::check_blocking))
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| DbSnapshot {
                writable: Err("檢查異常結束".into()),
                ..DbSnapshot::default()
            }),
    )
}

/// 告警引擎的狀態、事件與通知的計數
async fn render_alerts(ctx: &Arc<ObsContext>, m: &metrics::Metrics, e: &mut Exposition) {
    let snap = match &ctx.alerts {
        Some(svc) => Some(svc.metrics().await),
        None => None,
    };
    e.gauge(
        "kairos_alerts_engine_enabled",
        "告警引擎是否已啟動（alerts.enabled 且 SQLite；1 是）",
        b(snap.is_some()),
    );
    let s = snap.unwrap_or_default();
    e.gauge(
        "kairos_alerts_engine_up",
        "告警引擎的判定週期在 60 秒內執行過（1 是；0 時 S06 顯示「告警引擎未在執行」）",
        b(s.running),
    );
    e.gauge(
        "kairos_alerts_engine_last_tick_timestamp_seconds",
        "告警引擎最近一個判定週期的時間（Unix 秒；還沒有時為 0）",
        s.last_tick_ms as f64 / 1000.0,
    );
    e.gauge(
        "kairos_alerts_engine_tick_duration_seconds",
        "告警引擎最近一個判定週期花的時間（秒）",
        s.tick_seconds,
    );
    e.gauge(
        "kairos_alerts_engine_warmup",
        "是否在告警暖機期中（1 是；期間不發主機離線通知）",
        b(s.warmup),
    );
    let fam = e.family(
        "kairos_alerts_open",
        Kind::Gauge,
        "未解決的告警事件數（含彙總的子事件），依嚴重度與狀態（firing：觸發中；acknowledged：已確認；recovered：已恢復待結案）",
    );
    for (si, sev) in metrics::ALERT_SEVERITIES.iter().enumerate() {
        for (ti, st) in ["firing", "acknowledged", "recovered"].iter().enumerate() {
            fam.sample(
                "kairos_alerts_open",
                &[("severity", sev), ("state", st)],
                s.open[si][ti] as f64,
            );
        }
    }
    let fired: Vec<(&str, u64)> = metrics::ALERT_SEVERITIES
        .iter()
        .zip(&m.alerts_fired)
        .map(|(s, c)| (*s, c.get()))
        .collect();
    e.counter_by(
        "kairos_alerts_fired_total",
        "這次啟動後建立的告警事件數（含彙總的子事件），依嚴重度",
        "severity",
        &fired,
    );
    let resolved: Vec<(&str, u64)> = metrics::ALERT_SEVERITIES
        .iter()
        .zip(&m.alerts_resolved)
        .map(|(s, c)| (*s, c.get()))
        .collect();
    e.counter_by(
        "kairos_alerts_resolved_total",
        "這次啟動後解決的告警事件數，依嚴重度",
        "severity",
        &resolved,
    );
    e.counter(
        "kairos_alerts_events_lagged_total",
        "告警變化的通知落後而少算的事件數（不為 0 時上面兩項偏低）",
        m.alerts_lagged.get(),
    );
    let fam = e.family(
        "kairos_alert_channels",
        Kind::Gauge,
        "通知管道數，依狀態（ok；failed：連續 3 次失敗，KRS-E5107；disabled）",
    );
    for (i, st) in ["ok", "failed", "disabled"].iter().enumerate() {
        fam.sample(
            "kairos_alert_channels",
            &[("state", st)],
            s.channels[i] as f64,
        );
    }
    let fam = e.family(
        "kairos_alert_notification_sends_total",
        Kind::Counter,
        "通知的發送次數（一次 SMTP 或 HTTP 請求；合併的多則算一次；不含測試通知），依管道種類與結果",
    );
    for (kind, result, n) in m.alert_sends() {
        fam.sample(
            "kairos_alert_notification_sends_total",
            &[("kind", kind), ("result", result)],
            n as f64,
        );
    }
    let fam = e.family(
        "kairos_alert_notifications_total",
        Kind::Counter,
        "通知的最後狀態（sent：送出；failed：放棄；dropped：管道已停用或刪除），依管道種類",
    );
    for (kind, state, n) in m.alert_deliveries() {
        fam.sample(
            "kairos_alert_notifications_total",
            &[("kind", kind), ("state", state)],
            n as f64,
        );
    }
}

/// 以指定的資料庫檢查結果產生 `/metrics`（測試可直接呼叫）
pub async fn render_with(ctx: &Arc<ObsContext>, db: Option<&DbSnapshot>) -> String {
    let m = metrics::global();
    let now = Utc::now();
    let mut e = Exposition::new();
    let c = &ctx.config;

    // ---- 服務 ----
    e.family(
        "kairos_build_info",
        Kind::Gauge,
        "Kairos 的版本與建置的 commit（值固定為 1）",
    )
    .sample(
        "kairos_build_info",
        &[
            ("version", env!("CARGO_PKG_VERSION")),
            ("commit", env!("KAIROS_BUILD_COMMIT")),
        ],
        1.0,
    );
    e.gauge(
        "kairos_start_time_seconds",
        "啟動時間（Unix 秒）",
        ctx.started.timestamp() as f64,
    );
    e.gauge(
        "kairos_shutting_down",
        "是否正在關機（1 是）",
        b(ctx.shutdown.started_at().is_some()),
    );
    let (ready, _) = readiness(ctx, db, false).await;
    e.gauge(
        "kairos_ready",
        "與 /readyz 相同的就緒判斷（1 就緒）",
        b(ready),
    );

    // ---- 連線與狀態表 ----
    let (agents, connected) = ctx.registry.sizes();
    let (online, _) = ctx.registry.counts();
    e.gauge(
        "kairos_agent_connections",
        "已通過 hello 的 agent 連線數",
        connected as f64,
    );
    e.gauge(
        "kairos_agent_connections_max",
        "同時連線上限（server.max_connections）",
        c.server.max_connections as f64,
    );
    e.gauge("kairos_agents_online", "線上的 agent 數", online as f64);
    e.gauge(
        "kairos_registry_entries",
        "狀態表的筆數（線上與 24 小時內離線的 agent）",
        agents as f64,
    );
    e.gauge(
        "kairos_registry_max_entries",
        "狀態表上限（limits.max_agents）",
        ctx.registry.max_agents() as f64,
    );
    e.counter(
        "kairos_registry_evictions_total",
        "狀態表已滿時淘汰離線最久的 agent 的次數",
        m.registry_evictions.get(),
    );
    e.counter(
        "kairos_registry_full_rejections_total",
        "狀態表已滿且沒有可淘汰者而拒絕新 agent 的次數（KRS-E3201 的情境）",
        m.registry_full.get(),
    );
    e.counter(
        "kairos_agent_online_total",
        "agent 上線（新上線、重新上線、離線後恢復）的次數",
        m.online.get(),
    );
    let disconnects: Vec<(&str, u64)> = OFFLINE_REASONS
        .iter()
        .zip(&m.disconnects)
        .map(|(r, c)| (*r, c.get()))
        .collect();
    e.counter_by(
        "kairos_agent_disconnects_total",
        "agent 離線的次數，依原因（idle_timeout：超過離線門檻；agent_closed：agent 關閉連線或網路中斷；server_shutdown：Kairos 關機；certificate_rejected：連線中重新驗證憑證被拒；write_unknown：report 寫入結果未知而關閉連線；與 Webhook 的 offline_reason 相同）。由內部事件匯流排計數，與告警引擎同一個來源",
        "reason",
        &disconnects,
    );
    e.counter(
        "kairos_agent_superseded_total",
        "同一個 agent_id 的新連線取代舊連線的次數",
        m.superseded.get(),
    );
    e.counter(
        "kairos_agent_auth_failures_total",
        "hello 的 agent_id 與用戶端憑證不符而關閉連線的次數（KRS-W1102）",
        m.agent_auth_failed.get(),
    );
    e.counter(
        "kairos_host_events_lagged_total",
        "狀態表變化通知落後而少算的事件數（不為 0 時上線與離線的計數偏低）",
        m.host_events_lagged.get(),
    );
    let snap = ctx.per_ip.snapshot(Instant::now()).rejections;
    e.counter_by(
        "kairos_connections_rejected_total",
        "在 TLS 握手前被拒的連線，依原因（per_ip：每 IP 上限 KRS-W2201；rate：每來源新連線速率；handshake_limit：握手中上限 KRS-W2204）",
        "reason",
        &[
            ("per_ip", snap.per_ip.total),
            ("rate", snap.per_source_rate.total),
            ("handshake_limit", snap.global.handshake_limit_rejected),
        ],
    );
    e.counter(
        "kairos_accept_pauses_total",
        "達到 server.max_connections 而暫停接受新連線的次數（KRS-W2204）",
        snap.global.pauses,
    );
    e.family(
        "kairos_accept_paused_seconds_total",
        Kind::Counter,
        "暫停接受新連線的累計秒數",
    )
    .sample(
        "kairos_accept_paused_seconds_total",
        &[],
        snap.global.paused.as_secs_f64(),
    );
    e.gauge(
        "kairos_accept_paused",
        "目前是否暫停接受新連線（1 是）",
        b(snap.global.paused_now),
    );
    e.counter_by(
        "kairos_report_rate_limited_total",
        "report 速率超過上限而回 storage_unavailable 的次數，依計數的單位（identity：mTLS 的憑證身分；ip：未啟用 mTLS 時的來源 IP）",
        "scope",
        &[
            ("identity", m.report_rate_limited_identity.get()),
            ("ip", m.report_rate_limited_ip.get()),
        ],
    );

    // ---- 告警（kairos/ALERTS.md；標籤只有嚴重度、狀態、管道種類的有限集合） ----
    render_alerts(ctx, m, &mut e).await;

    // ---- report 寫入 ----
    let t = ctx.write_stats.totals();
    e.counter(
        "kairos_reports_received_total",
        "收到並進入處理的 report 數（下列各結果的合計）",
        t.written + t.duplicate + t.storage_unavailable + t.unknown + t.rate_limited,
    );
    e.counter_by(
        "kairos_reports_total",
        "report 的結果（written：已寫入；duplicate：重複、已去除；storage_unavailable：確定未保存 KRS-E4101；unknown：寫入結果未知、已關閉連線；rate_limited：速率限制 KRS-W2203）",
        "result",
        &[
            ("written", t.written),
            ("duplicate", t.duplicate),
            ("storage_unavailable", t.storage_unavailable),
            ("unknown", t.unknown),
            ("rate_limited", t.rate_limited),
        ],
    );
    e.gauge(
        "kairos_report_write_queue_depth",
        "寫入佇列中等待 writer 的 report 數",
        ctx.queue.depth().unwrap_or(0) as f64,
    );
    e.gauge(
        "kairos_report_write_queue_capacity",
        "寫入佇列的容量",
        ctx.queue.capacity() as f64,
    );
    e.histogram(
        "kairos_report_ack_duration_seconds",
        "從 report 放入寫入佇列到寫入完成（之後回 ack）的時間",
        &ctx.write_stats.ack_latency.snapshot(),
    );
    e.gauge(
        "kairos_storage_unavailable",
        "最近 1 分鐘的 report 都沒有保存成功且有寫入失敗（1 是；/readyz 的 storage）",
        b(storage_unavailable(&ctx.write_stats)),
    );

    // ---- 資料庫與保留清除 ----
    if let Some(d) = db {
        e.gauge(
            "kairos_database_writable",
            "資料庫是否可寫（1 是；/readyz 的 database）",
            b(d.writable.is_ok()),
        );
        if let Some(used) = d.used_bytes {
            e.gauge(
                "kairos_database_used_bytes",
                "資料庫的使用量（不含空頁與 WAL）",
                used as f64,
            );
        }
        e.gauge(
            "kairos_database_file_bytes",
            "資料庫檔案的大小",
            d.file_bytes as f64,
        );
        e.gauge(
            "kairos_database_wal_bytes",
            "WAL 檔的大小",
            d.wal_bytes as f64,
        );
        e.gauge(
            "kairos_database_max_bytes",
            "總容量上限（storage.max_total_bytes；0 不限制）",
            c.storage.max_total_bytes as f64,
        );
        e.gauge(
            "kairos_storage_capacity_deleting",
            "已達容量上限、正在提早刪除最舊的資料（1 是；KRS-W4202）",
            b(d.capacity_deleting),
        );
        e.gauge(
            "kairos_retention_paused",
            "因系統日期異常暫停保留清除（1 是；KRS-W4201）",
            b(d.retention_paused),
        );
    }

    // ---- TLS 與憑證 ----
    let material = ctx.tls.current();
    e.gauge(
        "kairos_mtls_enabled",
        "agent 連線埠是否要求用戶端憑證（1 是）",
        b(material.auth_enabled()),
    );
    let rejected: Vec<(&str, u64)> = CERT_REJECTION_REASONS
        .iter()
        .zip(&m.cert_rejected)
        .map(|(r, c)| (*r, c.get()))
        .collect();
    e.counter_by(
        "kairos_client_cert_rejected_total",
        "握手或連線中重新驗證時被拒的用戶端憑證，依原因",
        "reason",
        &rejected,
    );
    let mut expiries: Vec<(String, String, DateTime<Utc>)> = material
        .expiries
        .iter()
        .map(|x| {
            (
                match x.kind {
                    ExpiryKind::ServerCert => "server_certificate".to_string(),
                    ExpiryKind::Crl => "crl".to_string(),
                },
                x.index.to_string(),
                x.at,
            )
        })
        .collect();
    // 同一種類的多份 CRL（不同檔案）以出現順序編號，不用路徑當標籤
    let mut crl_no = 0;
    for x in expiries.iter_mut() {
        if x.0 == "crl" {
            x.1 = crl_no.to_string();
            crl_no += 1;
        }
    }
    if let Some(svc) = &ctx.enroll {
        let cas = svc
            .db
            .read(|conn| Ok(crate::pki::store::list_cas(conn)?))
            .await
            .unwrap_or_default();
        let mut n = 0;
        for ca in cas
            .iter()
            .filter(|ca| ca.state == crate::pki::store::CA_ACTIVE)
        {
            if let Some(at) = ca.not_after.and_then(DateTime::from_timestamp_millis) {
                expiries.push(("issuing_ca".to_string(), n.to_string(), at));
                n += 1;
            }
        }
    }
    e.family(
        "kairos_certificate_expiry_timestamp_seconds",
        Kind::Gauge,
        "到期時間（Unix 秒；kind：server_certificate 伺服器憑證、crl 的 nextUpdate、issuing_ca 啟用中的簽發 CA）",
    );
    for (kind, index, at) in &expiries {
        e.sample(
            "kairos_certificate_expiry_timestamp_seconds",
            &[("kind", kind), ("index", index)],
            at.timestamp() as f64,
        );
    }
    e.family(
        "kairos_certificate_days_remaining",
        Kind::Gauge,
        "距離到期的天數（已過期為負值）",
    );
    for (kind, index, at) in &expiries {
        e.sample(
            "kairos_certificate_days_remaining",
            &[("kind", kind), ("index", index)],
            ((*at - now).num_seconds() as f64 / 86_400.0 * 100.0).floor() / 100.0,
        );
    }

    // ---- 註冊端點（8444） ----
    if let Some(svc) = &ctx.enroll {
        let s = &svc.stats;
        let get = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        let state = svc.mode_state();
        e.gauge_by(
            "kairos_enroll_mode",
            "註冊端點的模式（目前的模式為 1）",
            "mode",
            &["open", "open_paused", "renew_only", "closed"]
                .map(|mode| (mode, b(state.as_str() == mode))),
        );
        e.gauge(
            "kairos_enroll_listening",
            "8444 是否在監聽（1 是）",
            b(enroll_state(ctx) == "ok"),
        );
        e.counter(
            "kairos_enroll_http_requests_total",
            "註冊端點收到的請求數",
            get(&s.requests),
        );
        e.counter_by(
            "kairos_enroll_submissions_total",
            "註冊申請，依種類（enroll：新註冊；renew：過期續簽）",
            "kind",
            &[
                ("enroll", get(&s.enroll_requests)),
                ("renew", get(&s.renew_requests)),
            ],
        );
        e.counter_by(
            "kairos_enroll_rate_limited_total",
            "因速率限制延後（429）的請求，依種類（KRS-W2202）",
            "kind",
            &[
                ("enroll", get(&s.limited_enroll)),
                ("renew", get(&s.limited_renew)),
            ],
        );
        e.counter(
            "kairos_enroll_blocked_total",
            "來源被封鎖而拒絕的請求（KRS-W3005）",
            get(&s.blocked),
        );
        e.counter(
            "kairos_enroll_busy_total",
            "全域過載或佇列已滿而回 503 的請求（KRS-W2205、KRS-E3309）",
            get(&s.busy),
        );
        e.counter(
            "kairos_enroll_clock_rejected_total",
            "因主機時鐘偏差超過 5 分鐘而暫不受理的請求（KRS-W8302）",
            get(&s.clock_rejected),
        );
        e.counter(
            "kairos_enroll_pre_handshake_closed_total",
            "在 TLS 握手前關閉的連線（連線層的限流）",
            get(&s.pre_handshake_closed),
        );
        e.counter(
            "kairos_enroll_issued_total",
            "簽發的憑證數（新註冊、過期續簽與 8443 輪替）",
            get(&s.issued),
        );
        e.counter(
            "kairos_enroll_pending_created_total",
            "建立的待核准申請數",
            get(&s.pending_created),
        );
        let now_ms = svc.now_ms();
        e.gauge(
            "kairos_enroll_blocked_sources",
            "目前被封鎖的來源數",
            svc.limits.l7.blocked_count(now_ms) as f64,
        );
        if let Ok(n) = svc
            .db
            .read(|conn| Ok(crate::enroll::store::pending_count(conn)?))
            .await
        {
            e.gauge(
                "kairos_enroll_pending_requests",
                "待核准的申請數（註冊佇列）",
                n as f64,
            );
        }
    }

    // ---- 主控台 API 與探針 ----
    e.family(
        "kairos_api_requests_total",
        Kind::Counter,
        "主控台 API 的請求，依路由分類與狀態碼分類",
    );
    for (route, code, n) in m.api_requests() {
        e.sample(
            "kairos_api_requests_total",
            &[("route", route), ("code", code)],
            n as f64,
        );
    }
    let auth: Vec<(&str, u64)> = metrics::AUTH_FAILURES
        .iter()
        .zip(&m.api_auth_failures)
        .map(|(r, c)| (*r, c.get()))
        .collect();
    e.counter_by(
        "kairos_api_auth_failures_total",
        "主控台 API 的認證失敗（unauthorized 401、forbidden 403、rate_limited 429）",
        "reason",
        &auth,
    );
    e.family(
        "kairos_probe_requests_total",
        Kind::Counter,
        "/healthz、/readyz、/metrics 的請求，依結果",
    );
    for (endpoint, result, n) in m.probe_requests() {
        e.sample(
            "kairos_probe_requests_total",
            &[("endpoint", endpoint), ("result", result)],
            n as f64,
        );
    }

    // ---- 日誌 ----
    e.counter_by(
        "kairos_log_messages_total",
        "記錄的日誌行數，依等級",
        "level",
        &kp_obs::logfmt::line_counts(),
    );
    if let Some(s) = crate::logger::sink().and_then(|s| s.syslog.as_ref()) {
        let st = &s.stats;
        e.counter_by(
            "kairos_log_syslog_messages_total",
            "送往 syslog 的日誌行，依結果（dropped：佇列已滿或接收端無法連線）",
            "result",
            &[
                ("sent", st.sent.load(Ordering::Relaxed)),
                ("dropped", st.dropped.load(Ordering::Relaxed)),
            ],
        );
    }

    // ---- 行程 ----
    if let Some((rss, fds, cpu)) = process_stats() {
        e.gauge(
            "kairos_process_resident_memory_bytes",
            "常駐記憶體",
            rss as f64,
        );
        e.gauge("kairos_process_open_fds", "開啟的檔案描述元數", fds as f64);
        e.family(
            "kairos_process_cpu_seconds_total",
            Kind::Counter,
            "使用的 CPU 時間（使用者＋系統）",
        )
        .sample("kairos_process_cpu_seconds_total", &[], cpu);
    }
    if let Some(soft) = crate::nofile_soft_limit().filter(|s| *s != u64::MAX) {
        e.gauge(
            "kairos_process_max_fds",
            "可開啟的檔案描述元上限（soft limit）",
            soft as f64,
        );
    }
    e.finish()
}

// ---- HTTP -------------------------------------------------------------------------------------

/// 這個路徑是不是探針端點
pub fn is_probe_path(path: &str) -> bool {
    matches!(path, "/healthz" | "/readyz" | "/metrics")
}

fn text_response(
    status: StatusCode,
    content_type: &'static str,
    body: String,
) -> Response<ApiBody> {
    let mut resp = Response::new(ApiBody::Full(Some(Bytes::from(body))));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn json_response(status: StatusCode, body: &Value) -> Response<ApiBody> {
    let mut s = body.to_string();
    s.push('\n');
    text_response(status, "application/json; charset=utf-8", s)
}

/// 處理探針端點的請求（`path` 必須是 [`is_probe_path`]）
pub async fn handle(
    ctx: &Arc<ObsContext>,
    method: &Method,
    path: &str,
    query: Option<&str>,
    authorization: Option<&str>,
    peer: SocketAddr,
) -> Response<ApiBody> {
    let m = metrics::global();
    if method != Method::GET && method != Method::HEAD {
        let mut r = json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            &json!({"status": "method_not_allowed"}),
        );
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return r;
    }
    let mut resp = match path {
        "/healthz" => {
            m.probe(0, 0);
            if ctx.shutdown.started_at().is_some() {
                json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &json!({"status": "shutting_down"}),
                )
            } else {
                json_response(StatusCode::OK, &json!({"status": "ok"}))
            }
        }
        "/readyz" => {
            let enroll_required = query.is_some_and(|q| q.split('&').any(|p| p == "scope=enroll"));
            let db = ctx_db(ctx).await;
            let (ready, body) = readiness(ctx, db.as_ref(), enroll_required).await;
            m.probe(1, if ready { 0 } else { 1 });
            json_response(
                if ready {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                },
                &body,
            )
        }
        _ => {
            if !ctx.metrics_allowed(peer, authorization) {
                m.probe(2, 2);
                let (status, reason) = if ctx.metrics_token.is_some() {
                    (StatusCode::UNAUTHORIZED, "unauthorized")
                } else {
                    (StatusCode::FORBIDDEN, "forbidden")
                };
                let mut r = json_response(status, &json!({"status": reason}));
                if status == StatusCode::UNAUTHORIZED {
                    r.headers_mut().insert(
                        header::WWW_AUTHENTICATE,
                        HeaderValue::from_static("Bearer realm=\"kairos-metrics\""),
                    );
                }
                r
            } else {
                m.probe(2, 0);
                text_response(
                    StatusCode::OK,
                    kp_obs::prom::CONTENT_TYPE,
                    render(ctx).await,
                )
            }
        }
    };
    if method == Method::HEAD {
        *resp.body_mut() = ApiBody::Full(None);
    }
    resp
}

/// 純 HTTP 監聽（`observability.listen`）：在自己的執行緒與 current-thread runtime 上執行
pub struct ProbeServer {
    pub local_addr: SocketAddr,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProbeServer {
    /// 等待結束（關機時 accept 迴圈停止）
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 監聽 `addr` 並在背景服務；監聽失敗時回錯誤（啟動失敗，連接埠被佔用為衝突）
pub fn start_listener(ctx: Arc<ObsContext>, addr: &str) -> Result<ProbeServer, Error> {
    let listener = std::net::TcpListener::bind(addr.trim())
        .and_then(|l| l.set_nonblocking(true).map(|_| l))
        .map_err(|e| {
            Error::new(
                e.kind(),
                format!("無法監聽 observability.listen {addr}: {e}"),
            )
        })?;
    let local_addr = listener.local_addr()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .map_err(|e| Error::other(format!("無法建立探針的 runtime: {e}")))?;
    let thread = std::thread::Builder::new()
        .name("kairos-probe".into())
        .spawn(move || {
            runtime.block_on(async move {
                match tokio::net::TcpListener::from_std(listener) {
                    Ok(l) => serve(l, ctx).await,
                    Err(e) => log::error!("探針端點無法使用監聽的 socket: {e}"),
                }
            });
            runtime.shutdown_timeout(Duration::from_millis(200));
        })?;
    Ok(ProbeServer {
        local_addr,
        thread: Some(thread),
    })
}

async fn serve(listener: tokio::net::TcpListener, ctx: Arc<ObsContext>) {
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_PROBE_CONNECTIONS));
    let mut conns = tokio::task::JoinSet::new();
    let shutdown = ctx.shutdown.clone();
    loop {
        while conns.try_join_next().is_some() {}
        let permit = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            p = limit.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break },
        };
        let (socket, peer) = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let ctx = ctx.clone();
        conns.spawn(async move {
            let _permit = permit;
            let service = service_fn(move |req: Request<Incoming>| {
                let ctx = ctx.clone();
                async move {
                    let path = req.uri().path().to_string();
                    let resp = if is_probe_path(&path) {
                        let auth = req
                            .headers()
                            .get(header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok());
                        handle(&ctx, req.method(), &path, req.uri().query(), auth, peer).await
                    } else {
                        json_response(StatusCode::NOT_FOUND, &json!({"status": "not_found"}))
                    };
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_buf_size(16 * 1024)
                .keep_alive(true);
            let _ = tokio::time::timeout(
                Duration::from_secs(60),
                builder.serve_connection(TokioIo::new(socket), service),
            )
            .await;
        });
    }
    drop(listener);
    conns.shutdown().await;
}
