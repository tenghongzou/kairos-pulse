//! 指標轉出（商業審計 F7；規格與取捨見 `kairos/EXPORT.md`）。
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`settings`] | 實際生效的設定：環境變數 ＞ 資料庫（主控台）＞ `kairos.toml` ＞ 內建預設 |
//! | [`prom`] | Prometheus text 0.0.4／OpenMetrics 1.0 的輸出（`GET /api/v1/export/prometheus`） |
//! | [`otlp`] | OTLP/HTTP（protobuf）的編碼、推送工作與用戶端 |
//! | [`proto`] | 最小的 protobuf 編碼器 |
//!
//! 資料流：連線保存一筆 report 後（[`crate::connection::store_report`] 寫入成功），把 report 的指標交給
//! [`ExportHub::accept`]：
//! - Prometheus exporter 啟用時，保存每台主機最新的一筆（記憶體中；抓取時才轉成文字）。
//! - OTLP 啟用時放入有上限的佇列，由 [`otlp::run`] 定期批次推送。
//!
//! 兩者都停用時不複製 report（[`ExportHub::wants_samples`]），沒有額外成本。

pub mod otlp;
pub mod prom;
pub mod proto;
pub mod settings;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use kairos_proto::{Metric, ProcessList};
use kp_obs::prom::{Exposition, Kind};
use log::warn;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

use crate::alerts::secrets::SecretBox;
use crate::config::AppConfig;
use crate::shutdown::Shutdown;
use crate::storage::{RecordBody, ReportRecord};

/// OTLP 推送失敗：collector 無法連線、逾時、TLS 錯誤或回應 5xx／429（暫定代碼）
pub const CODE_OTLP_SEND: &str = "KRS-E4311";
/// collector 拒絕資料：HTTP 4xx（429 以外）或回應 `partial_success` 帶被拒的資料點（暫定代碼）
pub const CODE_OTLP_REJECTED: &str = "KRS-E4312";
/// OTLP 佇列已滿，依 `drop_policy` 丟棄資料（暫定代碼）
pub const CODE_OTLP_QUEUE_FULL: &str = "KRS-W4313";
/// 指標轉出的設定無效（主控台送出的值、標頭檔無法讀取；暫定代碼）
pub const CODE_EXPORT_CONFIG: &str = "KRS-E4314";
/// Prometheus exporter 未啟用（`GET /api/v1/export/prometheus` 回 404 `export_disabled`；暫定代碼）
pub const CODE_EXPORT_DISABLED: &str = "KRS-E4315";

/// v1 report 的記憶體單位（MB）換成 bytes
const BYTES_PER_MB: u64 = 1024 * 1024;

/// 一筆要轉出的 report：主機、取樣時間、指標與行程排行
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSample {
    pub agent_id: Arc<str>,
    pub hostname: Arc<str>,
    /// 取樣時間（report 的 `ts`，Unix 毫秒；無法解析時為收到的時間）
    pub ts_ms: i64,
    pub metrics: Vec<Metric>,
    pub processes: Option<ProcessList>,
}

impl ExportSample {
    /// 由要寫入的 report 建立。v1 的三個欄位換成對應的標準指標（`system.cpu.usage` 與
    /// `system.memory.total`／`used`，MB × 1048576，與 Kairos 的儲存相同）
    pub fn from_record(r: &ReportRecord) -> Self {
        let ts_ms = DateTime::parse_from_rfc3339(&r.ts)
            .map(|t| t.timestamp_millis())
            .unwrap_or_else(|_| r.recv_at.timestamp_millis());
        let metrics = match &r.body {
            RecordBody::Metrics { metrics } => metrics.clone(),
            RecordBody::Legacy {
                cpu_usage,
                total_memory,
                used_memory,
            } => {
                let cpu =
                    serde_json::Number::from_f64(f64::from(*cpu_usage)).unwrap_or_else(|| 0.into());
                vec![
                    Metric {
                        name: kairos_proto::METRIC_CPU_USAGE.into(),
                        value: cpu,
                        unit: "%".into(),
                        labels: Default::default(),
                    },
                    Metric::new(
                        kairos_proto::METRIC_MEMORY_TOTAL,
                        total_memory.saturating_mul(BYTES_PER_MB),
                        "By",
                    ),
                    Metric::new(
                        kairos_proto::METRIC_MEMORY_USED,
                        used_memory.saturating_mul(BYTES_PER_MB),
                        "By",
                    ),
                ]
            }
        };
        Self {
            agent_id: Arc::from(r.agent_id.as_str()),
            hostname: Arc::from(r.hostname.as_str()),
            ts_ms,
            metrics,
            processes: r.processes.clone(),
        }
    }

    /// OTLP 的資料點數（佇列與批次的計算單位）
    pub fn points(&self) -> usize {
        self.metrics.len().max(1)
    }

    /// 同一筆 report 的 `system.uptime`（秒）
    pub fn uptime_secs(&self) -> Option<u64> {
        self.metrics
            .iter()
            .find(|m| m.name == kairos_proto::METRIC_UPTIME && m.labels.is_empty())
            .and_then(|m| m.value.as_u64())
    }
}

/// 佇列
#[derive(Debug, Default)]
struct Queue {
    items: VecDeque<Arc<ExportSample>>,
    points: usize,
}

/// OTLP 推送的狀態（主控台 S21、`/metrics`）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OtlpStatus {
    pub last_attempt_ms: Option<i64>,
    pub last_success_ms: Option<i64>,
    /// 最近一次失敗：(產品代碼, 訊息, 時間)
    pub last_error: Option<(&'static str, String, i64)>,
    pub consecutive_failures: u32,
    /// 最近一次成功的請求的耗時與大小
    pub last_request_ms: u64,
    pub last_request_bytes: usize,
}

/// Prometheus 抓取的狀態
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromStatus {
    pub last_scrape_ms: Option<i64>,
    pub last_duration_ms: u64,
    pub last_hosts: usize,
    pub last_series: usize,
    pub last_bytes: usize,
}

/// 計數（這次啟動後）
#[derive(Debug, Default)]
pub struct ExportStats {
    pub otlp_sent_points: AtomicU64,
    pub otlp_requests_ok: AtomicU64,
    pub otlp_requests_failed: AtomicU64,
    /// 佇列已滿而丟棄的資料點
    pub otlp_dropped_queue_full: AtomicU64,
    /// 重試用完而丟棄的資料點
    pub otlp_dropped_failed: AtomicU64,
    /// collector 拒絕（4xx 或 `partial_success`）的資料點
    pub otlp_dropped_rejected: AtomicU64,
    pub prom_scrapes: AtomicU64,
}

/// 指標轉出的共用狀態（狀態表持有一份，連線、主控台與 OTLP 推送工作共用）
pub struct ExportHub {
    settings: watch::Sender<Arc<settings::Effective>>,
    prom_on: AtomicBool,
    otlp_on: AtomicBool,
    latest: Mutex<HashMap<Arc<str>, Arc<ExportSample>>>,
    queue: Mutex<Queue>,
    /// 佇列累積到一批時喚醒推送工作
    wake: Notify,
    pub stats: ExportStats,
    pub otlp_status: Mutex<OtlpStatus>,
    pub prom_status: Mutex<PromStatus>,
    /// 佇列已滿的 WARN 最近一次記錄的時間（每分鐘最多一次）
    full_warned: Mutex<Option<Instant>>,
    /// 主控台輸入的 OTLP 標頭的加解密（SQLite 時；`alerts-secrets.key`）
    pub secrets: Option<Arc<SecretBox>>,
    /// 主機的群組、顯示名稱等（OTLP 的 resource 屬性；SQLite 時由推送工作定期讀取）
    pub db_path: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for ExportHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportHub")
            .field("prom_on", &self.prom_on)
            .field("otlp_on", &self.otlp_on)
            .finish_non_exhaustive()
    }
}

/// 佇列已滿的 WARN 的間隔
const FULL_WARN_EVERY: Duration = Duration::from_secs(60);

impl ExportHub {
    pub fn new(
        eff: settings::Effective,
        secrets: Option<Arc<SecretBox>>,
        db_path: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        let hub = Arc::new(Self {
            prom_on: AtomicBool::new(false),
            otlp_on: AtomicBool::new(false),
            settings: watch::channel(Arc::new(eff.clone())).0,
            latest: Mutex::new(HashMap::new()),
            queue: Mutex::new(Queue::default()),
            wake: Notify::new(),
            stats: ExportStats::default(),
            otlp_status: Mutex::new(OtlpStatus::default()),
            prom_status: Mutex::new(PromStatus::default()),
            full_warned: Mutex::new(None),
            secrets,
            db_path,
        });
        hub.apply(eff);
        hub
    }

    /// 目前生效的設定
    pub fn effective(&self) -> Arc<settings::Effective> {
        self.settings.borrow().clone()
    }

    /// 設定變更的通知（OTLP 推送工作）
    pub fn subscribe(&self) -> watch::Receiver<Arc<settings::Effective>> {
        self.settings.subscribe()
    }

    /// 套用新的設定：停用 Prometheus 時清除最新值的快取，停用 OTLP 時清空佇列
    pub fn apply(&self, eff: settings::Effective) {
        let prom = eff.prometheus.enabled;
        let otlp = eff.otlp_active();
        self.prom_on.store(prom, Ordering::Relaxed);
        self.otlp_on.store(otlp, Ordering::Relaxed);
        if !prom {
            lock(&self.latest).clear();
        }
        if !otlp {
            let mut q = lock(&self.queue);
            q.items.clear();
            q.points = 0;
        }
        self.settings.send_replace(Arc::new(eff));
        self.wake.notify_one();
    }

    /// 是否需要 report 的內容（兩者都停用時連線不複製 report）
    pub fn wants_samples(&self) -> bool {
        self.prom_on.load(Ordering::Relaxed) || self.otlp_on.load(Ordering::Relaxed)
    }

    /// 已保存的一筆 report。`durable` 的 report 可能是補送的舊資料：最新值只在取樣時間不早於目前的一筆時
    /// 更新；OTLP 一律送出（帶原本的取樣時間，collector 端依時間排列）
    pub fn accept(&self, sample: Arc<ExportSample>) {
        if self.prom_on.load(Ordering::Relaxed) {
            let mut latest = lock(&self.latest);
            match latest.get(&sample.agent_id) {
                Some(cur) if cur.ts_ms > sample.ts_ms => {}
                _ => {
                    latest.insert(sample.agent_id.clone(), sample.clone());
                }
            }
        }
        if self.otlp_on.load(Ordering::Relaxed) {
            self.enqueue(sample);
        }
    }

    fn enqueue(&self, sample: Arc<ExportSample>) {
        let eff = self.effective();
        let max = eff.otlp.queue_max_points;
        let batch = eff.otlp.max_batch_points;
        let newest = eff.otlp.drop_policy == crate::config::export::DROP_NEWEST;
        let n = sample.points();
        let mut dropped = 0usize;
        let ready = {
            let mut q = lock(&self.queue);
            if q.points + n > max {
                if newest {
                    dropped = n;
                } else {
                    while q.points + n > max {
                        match q.items.pop_front() {
                            Some(old) => {
                                q.points -= old.points();
                                dropped += old.points();
                            }
                            None => break,
                        }
                    }
                }
            }
            if dropped != n || !newest {
                q.points += n;
                q.items.push_back(sample);
            }
            q.points >= batch
        };
        if dropped > 0 {
            self.stats
                .otlp_dropped_queue_full
                .fetch_add(dropped as u64, Ordering::Relaxed);
            let mut w = lock(&self.full_warned);
            if w.is_none_or(|t| t.elapsed() >= FULL_WARN_EVERY) {
                *w = Some(Instant::now());
                warn!(
                    event = "export.otlp_queue_full", code = CODE_OTLP_QUEUE_FULL;
                    "警告 {CODE_OTLP_QUEUE_FULL}：OTLP 佇列已滿（{max} 個資料點），依 drop_policy = \"{}\" 丟棄 {dropped} 個資料點。collector 無法連線或推送跟不上時發生；確認 collector 的狀態（主控台「系統設定 → 整合」），必要時調高 export.otlp.queue_max_points 或 max_batch_points（每分鐘最多記錄一次）",
                    eff.otlp.drop_policy
                );
            }
        }
        if ready {
            self.wake.notify_one();
        }
    }

    /// 佇列中的資料點數
    pub fn queue_points(&self) -> usize {
        lock(&self.queue).points
    }

    /// 取出一批：從最舊的開始，以 report 為單位，合計不超過 `max_points`（第一筆一定取出）
    pub fn take_batch(&self, max_points: usize) -> Vec<Arc<ExportSample>> {
        let mut q = lock(&self.queue);
        let mut out = Vec::new();
        let mut points = 0;
        while let Some(front) = q.items.front() {
            let n = front.points();
            if !out.is_empty() && points + n > max_points {
                break;
            }
            points += n;
            q.points -= n;
            if let Some(s) = q.items.pop_front() {
                out.push(s);
            }
        }
        out
    }

    /// 等待佇列累積到一批（或設定改變）
    pub async fn notified(&self) {
        self.wake.notified().await;
    }

    /// 每台主機最新的一筆（Prometheus exporter；複製參考計數）
    pub fn latest(&self) -> HashMap<Arc<str>, Arc<ExportSample>> {
        lock(&self.latest).clone()
    }

    /// 主機的資料被刪除時（`kairos/PRIVACY.md` 第 4 節）：丟掉記憶體中的最新值（之後的抓取不再輸出）
    /// 與 OTLP 佇列中尚未送出的這台主機的資料。回傳從佇列移除的資料點數
    pub fn forget(&self, agent_id: &str) -> usize {
        lock(&self.latest).remove(agent_id);
        let mut q = lock(&self.queue);
        let before = q.points;
        q.items.retain(|s| &*s.agent_id != agent_id);
        q.points = q.items.iter().map(|s| s.points()).sum();
        before - q.points
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 啟動時建立：讀取資料庫的設定（SQLite）、計算生效值、記錄摘要，並啟動 OTLP 推送工作
pub fn start(
    config: &AppConfig,
    data_dir: &Path,
    shutdown: Arc<Shutdown>,
) -> (Arc<ExportHub>, tokio::task::JoinHandle<()>) {
    let sqlite = config.storage.backend == crate::config::StorageBackend::Sqlite;
    let db_path =
        sqlite.then(|| crate::store::sqlite::SqliteOptions::from_config(config, data_dir).path);
    let secrets = if sqlite {
        match SecretBox::open(data_dir) {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                warn!(
                    "指標轉出：無法開啟 {}（主控台輸入的 OTLP 標頭無法使用）：{e}",
                    crate::alerts::secrets::KEY_FILE
                );
                None
            }
        }
    } else {
        None
    };
    let rows = db_path
        .as_deref()
        .map(settings::load_rows)
        .unwrap_or_default();
    let eff = settings::effective(config, &rows, secrets.as_deref());
    log_summary(&eff);
    let hub = ExportHub::new(eff, secrets, db_path);
    let task = tokio::spawn(otlp::run(hub.clone(), shutdown));
    (hub, task)
}

/// 啟動時的一行摘要與設定問題
fn log_summary(eff: &settings::Effective) {
    log::info!(
        "指標轉出：Prometheus exporter {}；OTLP {}",
        if eff.prometheus.enabled {
            "已啟用（GET /api/v1/export/prometheus）"
        } else {
            "未啟用"
        },
        if eff.otlp_active() {
            format!(
                "已啟用（{}，每 {} 秒，名稱 {}）",
                crate::alerts::secrets::mask_url(&eff.otlp.endpoint),
                eff.otlp.interval_secs,
                eff.otlp.naming
            )
        } else {
            "未啟用".to_string()
        }
    );
    for p in &eff.problems {
        warn!(event = "export.config", code = CODE_EXPORT_CONFIG; "警告 {CODE_EXPORT_CONFIG}：{p}");
    }
}

fn b(v: bool) -> f64 {
    if v { 1.0 } else { 0.0 }
}

/// `/metrics`（Kairos 自身的指標）中指標轉出的部分
pub fn render_self_metrics(hub: Option<&ExportHub>, e: &mut Exposition) {
    let Some(hub) = hub else {
        return;
    };
    let eff = hub.effective();
    let s = &hub.stats;
    e.gauge(
        "kairos_export_prometheus_enabled",
        "Prometheus exporter（/api/v1/export/prometheus）是否啟用（1 是）",
        b(eff.prometheus.enabled),
    );
    e.counter(
        "kairos_export_prometheus_scrapes_total",
        "這次啟動後 /api/v1/export/prometheus 的成功抓取次數",
        s.prom_scrapes.load(Ordering::Relaxed),
    );
    e.gauge(
        "kairos_export_otlp_enabled",
        "OTLP 推送是否啟用（1 是）",
        b(eff.otlp_active()),
    );
    e.gauge(
        "kairos_export_otlp_queue_points",
        "OTLP 佇列中等待推送的資料點數",
        hub.queue_points() as f64,
    );
    e.counter(
        "kairos_export_otlp_sent_points_total",
        "這次啟動後 collector 已接受的資料點數",
        s.otlp_sent_points.load(Ordering::Relaxed),
    );
    e.family(
        "kairos_export_otlp_requests_total",
        Kind::Counter,
        "這次啟動後的 OTLP 請求數（含重試），依結果",
    )
    .sample(
        "kairos_export_otlp_requests_total",
        &[("result", "ok")],
        s.otlp_requests_ok.load(Ordering::Relaxed) as f64,
    )
    .sample(
        "kairos_export_otlp_requests_total",
        &[("result", "failed")],
        s.otlp_requests_failed.load(Ordering::Relaxed) as f64,
    );
    e.family(
        "kairos_export_otlp_dropped_points_total",
        Kind::Counter,
        "這次啟動後丟棄的資料點數（queue_full：佇列已滿 KRS-W4313；failed：重試用完 KRS-E4311；rejected：collector 拒絕 KRS-E4312）",
    )
    .sample(
        "kairos_export_otlp_dropped_points_total",
        &[("reason", "queue_full")],
        s.otlp_dropped_queue_full.load(Ordering::Relaxed) as f64,
    )
    .sample(
        "kairos_export_otlp_dropped_points_total",
        &[("reason", "failed")],
        s.otlp_dropped_failed.load(Ordering::Relaxed) as f64,
    )
    .sample(
        "kairos_export_otlp_dropped_points_total",
        &[("reason", "rejected")],
        s.otlp_dropped_rejected.load(Ordering::Relaxed) as f64,
    );
    let last_ok = lock(&hub.otlp_status).last_success_ms.unwrap_or(0);
    e.gauge(
        "kairos_export_otlp_last_success_timestamp_seconds",
        "最近一次推送成功的時間（Unix 秒；還沒有時為 0）",
        last_ok as f64 / 1000.0,
    );
}

/// 目前的時間（Unix 毫秒）
pub(crate) fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}
