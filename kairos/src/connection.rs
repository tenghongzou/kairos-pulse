//! 單一連線的處理：TLS 握手 → 等待 hello → 已註冊狀態的訊息迴圈。

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use log::{debug, trace};
use std::future::Future;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::UnixTime;
use tokio_rustls::server::TlsStream;
use tokio_util::codec::Framed;

use crate::auth::{BindingError, BindingPolicy, cert_matches_agent_id};
use crate::config::TimeoutSettings;
use crate::per_ip::{ConnectionSlot, source_key};
use crate::protocol::{
    ClientMsg, ErrorCode, FEATURE_DEREGISTER, FEATURE_RENEW, Frame, HelloExt, HostInfo,
    INTENT_DEREGISTER, MAX_CONSECUTIVE_BAD_REQUESTS, MAX_LINE_LENGTH, METRIC_CPU_USAGE,
    METRIC_MEMORY_TOTAL, METRIC_MEMORY_USED, Metric, NdjsonCodec, Proto, ProtoRangeError,
    SERVER_FEATURES, SUPPORTED_PROTO, ServerMsg, Session, clamp_heartbeat_interval_ms,
    client_proto_range, hello_seq_of_invalid, message_type, negotiate, normalize_agent_id,
    offline_after_ms, parse_client_msg, server_cpu_usage, unsupported_version_error,
    validate_client_msg, validate_features, validate_hello, validate_host_info, validate_metrics,
    validate_report,
};
use crate::rate::{
    MAX_REPORT_PACING_DELAY, Pace, REPORT_BURST, REPORT_RATE_PER_SEC, ReportLimiter, ReportSource,
    TokenBucket,
};
use crate::registry::{
    AssetSummary, BACKFILL_LAG_MS, ClientChain, FsLast, HelloInfo, IoLast, KickReason, LastReport,
    RegisterKind, Registry, ReportDetail, Touch,
};
use crate::shutdown::{CONNECTIONS_DEADLINE, Shutdown, drain_limit};
use crate::stats::WriteOutcome;
use crate::storage::{DedupKey, RecordBody, ReportRecord, ReportSink, WriteStatus, fmt_time};
use crate::throttle::{Category, Key, LogThrottle};
use crate::throttled;
use crate::tls::{
    MSG_CERTIFICATE_REJECTED, RevocationFailure, TlsState, revocation_failure, verify_chain,
};
use kairos_proto::{
    DRAIN_REASON_NODE_DRAINING, FEATURE_CLUSTER, FEATURE_INVENTORY, FEATURE_PROCESSES,
    INVENTORY_MIN_INTERVAL_MS, InventoryReport, drain_reconnect_within_ms, validate_inventory,
    validate_process_list,
};

/// 送出 close_notify 後，讀取並丟棄對方剩餘資料的最長時間（平常；關機期間另有更短的上限）。
/// 若在對方仍在送資料時直接關閉 socket，作業系統會送出 RST，對方可能因此收不到最後的錯誤訊息。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// 握手失敗後，送出 alert 與 FIN 再讀取丟棄對方資料的上限。
/// 失敗多半來自掃描或設定錯誤，時間取短，避免被大量觸發時長時間佔用連線名額。
const HANDSHAKE_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// 每條連線的訊息速率上限（所有訊息，含無效行；R2-K12）：可連續處理的則數
pub const MESSAGE_BURST: f64 = 50.0;

/// 每條連線每秒最多處理的訊息數。Pulse 正常每秒最多送出約 2 則，不會觸及。
/// 超過時延後處理（期間不讀取，由 TCP 對送出端施加背壓），每則最多延後 1 / 此值 秒。
pub const MESSAGES_PER_SEC: f64 = 20.0;

/// 單則訊息因訊息速率上限而延後的上限（= 1 / [`MESSAGES_PER_SEC`]）
pub const MAX_MESSAGE_PACING_DELAY: Duration = Duration::from_millis(50);

/// 等待寫檔逾時、但 writer 已開始寫入這筆（無法取消）時，額外等待的上限
const CLAIMED_WRITE_GRACE: Duration = Duration::from_secs(1);

/// 等待寫檔期間更新本連線 last_seen 的間隔。等待期間同一連線的 heartbeat 排在後面無法處理，
/// 最長可達 `write_secs` + 寬限 = 7 秒，超過最小的離線門檻 5 秒；定期更新避免 agent 被短暫標記離線。
/// 必須小於最小離線門檻（5 秒）。
const STORE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// 從收到 report 到送出回應（`ack` / `storage_unavailable`，或結果未知時斷線）的最壞延遲：
///
/// ```text
/// 訊息速率延後 MAX_MESSAGE_PACING_DELAY 0.05 s
/// + report 限速延後 MAX_REPORT_PACING_DELAY 0.5 s
/// + 等待寫檔 timeouts.write_secs     ≤ MAX_WRITE_SECS 6 s
/// + 寫入已開始時的寬限 CLAIMED_WRITE_GRACE 1 s
/// = 7.55 s ≤ 8 s
/// ```
///
/// 等待期間同一連線的 heartbeat 排在後面拿不到 `ack`。Pulse 的讀取閒置為
/// `max(2m, m + 10 s)`（m 為 Pulse 的心跳間隔）：Pulse 最後一次收到伺服器訊息後，
/// 最晚 m 秒內會送出下一則訊息，伺服器再於 8 秒內回應，`m + 8 s < m + 10 s`，
/// 因此對任何心跳間隔都不會被誤判為斷線。
pub const MAX_REPORT_RESPONSE_DELAY: Duration = Duration::from_secs(8);

// 編譯期檢查延遲預算（以毫秒計算）
const _: () = {
    let total_ms = MAX_MESSAGE_PACING_DELAY.as_millis()
        + MAX_REPORT_PACING_DELAY.as_millis()
        + (crate::config::MAX_WRITE_SECS as u128) * 1000
        + CLAIMED_WRITE_GRACE.as_millis();
    assert!(total_ms <= MAX_REPORT_RESPONSE_DELAY.as_millis());
    assert!(MAX_MESSAGE_PACING_DELAY.as_millis() * (MESSAGES_PER_SEC as u128) >= 1000);
};

/// 讀取並丟棄 `stream` 的資料，直到對方關閉、發生錯誤或超過 `limit`；回傳丟棄的 bytes 數，逾時時為 `None`
async fn drain<S: AsyncRead + Unpin>(stream: &mut S, limit: Duration) -> Option<usize> {
    let mut buf = vec![0u8; 16 * 1024];
    timeout(limit, async {
        let mut total = 0usize;
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break total,
                Ok(n) => total += n,
            }
        }
    })
    .await
    .ok()
}

/// 所有連線共用的資源
pub struct ConnContext {
    pub registry: Arc<Registry>,
    pub reports: ReportSink,
    pub timeouts: TimeoutSettings,
    /// 伺服器關機狀態（取消訊號與 T0）
    pub shutdown: Arc<Shutdown>,
    /// 目前生效的 TLS 設定（熱重載時替換）；hello 時依它判斷是否要求憑證綁定
    pub tls: Arc<TlsState>,
    /// 可能被掃描大量觸發的日誌的節流器
    pub throttle: Arc<LogThrottle>,
    /// mTLS 名稱綁定的規則
    pub binding: BindingPolicy,
    /// 跨連線共用的 report 限速（R2-K3）
    pub report_limiter: Arc<ReportLimiter>,
    /// 註冊服務：8443 的 `renew`、`deregister`、除役後的 hello 判定（SQLite 時）
    pub enroll: Option<Arc<crate::enroll::EnrollService>>,
    /// 資產的寫入佇列（SQLite 時）。有值時宣告 `inventory`、`processes`（METRICS.md 第 7.1 節）
    pub assets: Option<crate::store::AssetSink>,
    /// 個資政策（`[privacy]`，`kairos/PRIVACY.md`）：保存前轉換主機名稱、來源位址、行程與資產
    pub privacy: Arc<crate::privacy::PrivacyPolicy>,
    /// feature `cluster`：節點名稱與關機時送出的 `drain` 計數
    pub cluster: ClusterContext,
}

/// feature `cluster`（PROTOCOL.md「叢集與排空」）的伺服器端狀態
#[derive(Debug)]
pub struct ClusterContext {
    /// `hello_ack.server_node`（`cluster.node_id`，預設由主機名稱導出）
    pub node_id: Arc<str>,
    /// 關機時送出 `drain` 的連線數（關機日誌 `KRS-I8407`）
    pub drains_sent: std::sync::atomic::AtomicU64,
    /// 關機時的 `reconnect_within_ms`（第一條送 `drain` 的連線依線上數計算一次，其他連線沿用）
    drain_window: std::sync::OnceLock<u32>,
}

impl ClusterContext {
    pub fn new(node_id: impl Into<Arc<str>>) -> Self {
        Self {
            node_id: node_id.into(),
            drains_sent: std::sync::atomic::AtomicU64::new(0),
            drain_window: std::sync::OnceLock::new(),
        }
    }

    /// 關機排空的分散時間：`max(10000, 線上連線數 × 5)` ms（只計算一次）
    pub fn drain_window(&self, registry: &Registry) -> u32 {
        *self
            .drain_window
            .get_or_init(|| drain_reconnect_within_ms(registry.counts().0))
    }
}

/// Kairos 宣告的 feature：[`SERVER_FEATURES`]，儲存後端保存資產與行程時（SQLite）另加 `inventory`、
/// `processes`（與 `renew` 相同，不在共用的 `SERVER_FEATURES` 中；METRICS.md 第 7.1 節）
pub fn server_features(assets: bool) -> Vec<&'static str> {
    let mut f = SERVER_FEATURES.to_vec();
    if assets {
        f.push(FEATURE_INVENTORY);
        f.push(FEATURE_PROCESSES);
    }
    f
}

type Transport = Framed<TlsStream<TcpStream>, NdjsonCodec>;

/// 處理完一則訊息後是否繼續
enum Flow {
    Continue,
    Close,
}

/// 已註冊狀態下處理一則訊息的結果
struct Handled {
    flow: Flow,
    /// 是否為合法訊息（hello、heartbeat、report，含 invalid_report）；只有合法訊息會重置閒置計時
    alive: bool,
}

impl Handled {
    fn alive(flow: Flow) -> Self {
        Self { flow, alive: true }
    }

    fn not_alive(flow: Flow) -> Self {
        Self { flow, alive: false }
    }
}

/// 讀到的一行經初步分類後的結果
enum Incoming {
    Msg(ClientMsg),
    TooLong,
    /// 無效訊息；`seq` 只在能辨識為 hello 並取得 seq 時才有值（F5），回應的 `bad_request` 帶回它
    BadRequest {
        message: String,
        seq: Option<u64>,
    },
    /// 無法解析或不合規定的 `renew_request`／`deregister`（`kind`）。協商了對應的 feature 時回
    /// `bad_request`；沒有協商時與舊版相同，回 `unknown_type`
    InvalidLifecycle {
        kind: String,
        message: String,
    },
}

/// `renew_request`／`deregister` 的 `type`（PROTOCOL.md「憑證輪替與除役」）
fn is_lifecycle_type(kind: &str) -> bool {
    // inventory（feature `inventory`，crates/kairos-proto/METRICS.md）：協商了時格式錯誤回 bad_request，
    // 沒有協商時與舊版相同，回 unknown_type
    matches!(kind, "renew_request" | "deregister" | "inventory")
}

/// 8443 的註冊 feature（`renew`、`deregister`）是否使用：協商為 v2、Kairos 有註冊服務、client 也宣告，
/// **且這條連線以用戶端憑證證明了 `agent_id`**（PROTOCOL.md「憑證輪替與除役」）。未啟用 mTLS 時
/// `agent_id` 是自行宣告的，若接受 `renew_request` 就等於替任何人簽發任意 `agent_id` 的憑證
fn lifecycle_features(
    authenticated: bool,
    v2: bool,
    has_enroll: bool,
    client_features: &[String],
) -> (bool, bool) {
    let both =
        |f: &str| authenticated && v2 && has_enroll && client_features.iter().any(|c| c == f);
    (both(FEATURE_RENEW), both(FEATURE_DEREGISTER))
}

fn classify(frame: Frame) -> Incoming {
    match frame {
        Frame::TooLong => Incoming::TooLong,
        Frame::InvalidUtf8 => Incoming::BadRequest {
            message: "line is not valid UTF-8".to_string(),
            seq: None,
        },
        Frame::Line(line) => match parse_client_msg(&line) {
            Ok(msg) => match validate_client_msg(&msg) {
                Ok(()) => Incoming::Msg(msg),
                Err(message) => match &msg {
                    ClientMsg::RenewRequest(_) => Incoming::InvalidLifecycle {
                        kind: "renew_request".into(),
                        message,
                    },
                    ClientMsg::Deregister(_) => Incoming::InvalidLifecycle {
                        kind: "deregister".into(),
                        message,
                    },
                    ClientMsg::Inventory(_) => Incoming::InvalidLifecycle {
                        kind: "inventory".into(),
                        message,
                    },
                    _ => Incoming::BadRequest {
                        seq: match &msg {
                            ClientMsg::Hello { seq, .. } => Some(*seq),
                            _ => None,
                        },
                        message,
                    },
                },
            },
            Err(message) => match message_type(&line) {
                Some(kind) if is_lifecycle_type(&kind) => {
                    Incoming::InvalidLifecycle { kind, message }
                }
                _ => Incoming::BadRequest {
                    seq: hello_seq_of_invalid(&line),
                    message,
                },
            },
        },
    }
}

/// 資產資訊不合法（METRICS.md 第 14 節；設計包 `copywriting.md` 範例 96，2026-10-15 正式分配）
pub const CODE_INVALID_INVENTORY: &str = "KRS-W2108";

/// 一個 MB（agents.json 的 `total_memory`／`used_memory` 單位，與 v1 report 相同）
const BYTES_PER_MB: u64 = 1024 * 1024;

/// agents.json 的 `last_report`：v1 report 照舊；v2 的指標 report 取沒有 label 的
/// `system.cpu.usage`、`system.memory.total`／`used`（bytes 換算成 MB），另記指標數
fn last_report_of(record: &ReportRecord) -> LastReport {
    let (cpu_usage, total_memory, used_memory, metrics) = match &record.body {
        RecordBody::Legacy {
            cpu_usage,
            total_memory,
            used_memory,
        } => (
            Some(*cpu_usage),
            Some(*total_memory),
            Some(*used_memory),
            None,
        ),
        RecordBody::Metrics { metrics } => {
            let find = |name: &str| {
                metrics
                    .iter()
                    .find(|m| m.name == name && m.labels.is_empty())
                    .map(|m| &m.value)
            };
            let mb = |name: &str| {
                find(name)
                    .and_then(|v| v.as_u64())
                    .map(|b| b / BYTES_PER_MB)
            };
            (
                find(METRIC_CPU_USAGE)
                    .and_then(|v| v.as_f64())
                    .map(server_cpu_usage),
                mb(METRIC_MEMORY_TOTAL),
                mb(METRIC_MEMORY_USED),
                Some(metrics.len()),
            )
        }
    };
    let detail = match &record.body {
        RecordBody::Metrics { metrics } => report_detail(metrics),
        RecordBody::Legacy { .. } => ReportDetail::default(),
    };
    LastReport {
        recv_at: fmt_time(record.recv_at),
        seq: record.seq,
        ts: record.ts.clone(),
        cpu_usage,
        total_memory,
        used_memory,
        metrics,
        detail,
    }
}

/// report 中 F4 指標的摘要（registry 的 `LastReport`：告警與主控台的「目前數值」）。速率由 registry
/// 以上一筆算出；沒有 `system.filesystem.usage` 的掛載點以 total 與 available 推算（METRICS.md 第 9.2 節）
fn report_detail(metrics: &[Metric]) -> ReportDetail {
    use kairos_proto::{
        METRIC_CPU_LOAD_1M, METRIC_CPU_LOAD_5M, METRIC_CPU_LOAD_15M, METRIC_DISK_READ,
        METRIC_DISK_WRITTEN, METRIC_FILESYSTEM_AVAILABLE, METRIC_FILESYSTEM_TOTAL,
        METRIC_FILESYSTEM_USAGE, METRIC_FILESYSTEM_USED, METRIC_MEMORY_AVAILABLE,
        METRIC_NETWORK_RECEIVED, METRIC_NETWORK_TRANSMITTED, METRIC_PROCESS_COUNT,
        METRIC_SWAP_TOTAL, METRIC_SWAP_USED, METRIC_UPTIME, standard_metric,
    };
    let mut d = ReportDetail::default();
    // 只採用單位與標準相符的標準指標（自訂或單位不符的同名指標不採用）
    let std_ok = |m: &Metric| standard_metric(&m.name).is_some_and(|s| s.unit == m.unit);
    let fs_of = |d: &mut ReportDetail, m: &Metric| -> Option<usize> {
        let mp = m.labels.get("mountpoint")?;
        let i = match d.filesystems.iter().position(|f| &f.mountpoint == mp) {
            Some(i) => i,
            None => {
                if d.filesystems.len() >= 64 {
                    return None;
                }
                d.filesystems.push(FsLast {
                    mountpoint: mp.clone(),
                    fstype: m.labels.get("fstype").cloned(),
                    ..Default::default()
                });
                d.filesystems.len() - 1
            }
        };
        Some(i)
    };
    let io_of = |list: &mut Vec<IoLast>, m: &Metric| -> Option<usize> {
        let dev = m.labels.get("device")?;
        match list.iter().position(|f| &f.device == dev) {
            Some(i) => Some(i),
            None if list.len() < 64 => {
                list.push(IoLast {
                    device: dev.clone(),
                    ..Default::default()
                });
                Some(list.len() - 1)
            }
            None => None,
        }
    };
    for m in metrics.iter().filter(|m| std_ok(m)) {
        let u = m.value.as_u64();
        let f = m.value.as_f64();
        match m.name.as_str() {
            METRIC_UPTIME if m.labels.is_empty() => d.uptime_secs = u,
            METRIC_MEMORY_AVAILABLE if m.labels.is_empty() => d.memory_available = u,
            METRIC_SWAP_TOTAL if m.labels.is_empty() => d.swap_total = u,
            METRIC_SWAP_USED if m.labels.is_empty() => d.swap_used = u,
            METRIC_CPU_LOAD_1M if m.labels.is_empty() => d.load_1m = f,
            METRIC_CPU_LOAD_5M if m.labels.is_empty() => d.load_5m = f,
            METRIC_CPU_LOAD_15M if m.labels.is_empty() => d.load_15m = f,
            METRIC_PROCESS_COUNT if m.labels.is_empty() => d.process_count = u,
            METRIC_FILESYSTEM_TOTAL => {
                if let Some(i) = fs_of(&mut d, m) {
                    d.filesystems[i].total = u;
                }
            }
            METRIC_FILESYSTEM_AVAILABLE => {
                if let Some(i) = fs_of(&mut d, m) {
                    d.filesystems[i].available = u;
                }
            }
            METRIC_FILESYSTEM_USED => {
                if let Some(i) = fs_of(&mut d, m) {
                    d.filesystems[i].used = u;
                }
            }
            METRIC_FILESYSTEM_USAGE => {
                if let Some(i) = fs_of(&mut d, m) {
                    d.filesystems[i].usage = f;
                }
            }
            METRIC_NETWORK_RECEIVED => {
                if let Some(i) = io_of(&mut d.network, m) {
                    d.network[i].rx = u;
                }
            }
            METRIC_NETWORK_TRANSMITTED => {
                if let Some(i) = io_of(&mut d.network, m) {
                    d.network[i].tx = u;
                }
            }
            METRIC_DISK_READ => {
                if let Some(i) = io_of(&mut d.disks, m) {
                    d.disks[i].rx = u;
                }
            }
            METRIC_DISK_WRITTEN => {
                if let Some(i) = io_of(&mut d.disks, m) {
                    d.disks[i].tx = u;
                }
            }
            _ => {}
        }
    }
    for fs in &mut d.filesystems {
        if fs.usage.is_none()
            && let (Some(t), Some(a)) = (fs.total, fs.available)
        {
            fs.usage = crate::store::derive::estimated_usage(t as f64, a as f64);
            fs.estimated = fs.usage.is_some();
        }
        if fs.used.is_none()
            && fs.estimated
            && let (Some(t), Some(a)) = (fs.total, fs.available)
        {
            fs.used = Some(t.saturating_sub(a));
        }
    }
    d
}

/// 計算 `now + duration` 的期限；溢位時退回較短的期限，避免 `Instant + Duration` panic
fn deadline_after(duration: Duration) -> Instant {
    const FALLBACK: Duration = Duration::from_secs(24 * 60 * 60);
    let now = Instant::now();
    now.checked_add(duration)
        .or_else(|| now.checked_add(FALLBACK))
        .unwrap_or(now)
}

/// 合法 report 的保存結果
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoreOutcome {
    /// 已寫入資料檔並更新 last_report；內含等待期間各次 last_seen 更新的合併結果
    /// （任何一次為 [`Touch::Revived`] 即為 `Revived`）
    Stored(Touch),
    /// 本連線已被取代，未寫檔也未更新任何狀態
    Stale,
    /// 寫檔失敗或逾時，資料確定未保存（不更新 last_report）
    Unavailable(String),
    /// 逾時時 writer 已開始寫入這筆，無法確定是否已保存。不能回 `storage_unavailable`（未保存），
    /// 也不能回 `ack`，呼叫端應不回應直接關閉連線
    Unknown(String),
}

/// 保存一筆合法 report，順序保證 agents.json 與資料檔一致：
/// 1. 以 conn_id 確認仍是現役連線（並更新 last_seen），被取代的舊連線不寫檔；
/// 2. 交給 writer 並等待「已寫入並 flush」的回覆，整段受 `write_timeout` 限制；
///    逾時時先取消這筆要求，writer 保證不會再寫入，才回 [`StoreOutcome::Unavailable`]；
///    writer 已開始寫入而無法取消時，再等一小段，仍無結果則回 [`StoreOutcome::Unknown`]；
/// 3. 寫入成功後才更新 last_report，並再更新一次 last_seen（等待寫檔期間收不到 heartbeat）。
///
/// `record.dedup` 為 durable report 的去重鍵（A5，[`ReportRecord::with_dedup`]）：已保存過的那筆不再
/// 寫入（[`WriteStatus::Duplicate`]），同樣回 [`StoreOutcome::Stored`]（連線回 `ack`），但不更新
/// last_report。durable report 可能是補送的舊資料，只在 `ts` 不早於目前的 last_report 時更新。
///
/// 各次更新 last_seen 中只要有一次讓 agent 從離線恢復，就把 `revived` 設為 `true`（不論保存結果），
/// 呼叫端據此記錄「恢復上線」（R2-K13）。
pub(crate) async fn store_report(
    registry: &Registry,
    sink: &ReportSink,
    conn_id: u64,
    record: ReportRecord,
    write_timeout: Duration,
    revived: &AtomicBool,
) -> StoreOutcome {
    let touch = registry.touch(
        &record.agent_id,
        conn_id,
        std::time::Instant::now(),
        Utc::now(),
    );
    match touch {
        Touch::Stale => return StoreOutcome::Stale,
        Touch::Revived => revived.store(true, Ordering::Relaxed),
        Touch::Updated => {}
    }
    let agent_id = record.agent_id.clone();
    let last = last_report_of(&record);
    // 指標轉出（Prometheus exporter、OTLP；kairos/EXPORT.md）：啟用時才複製 report 的內容
    let export = registry
        .export_hub()
        .filter(|h| h.wants_samples())
        .map(|h| {
            (
                h,
                Arc::new(crate::export::ExportSample::from_record(&record)),
            )
        });
    let timed_out = || format!("寫入未在 {} 秒內完成", write_timeout.as_secs_f64());
    let deadline = deadline_after(write_timeout);
    // 每筆的結果記入寫入統計（系統健康的「report 寫入」）
    let stats = sink.stats().clone();
    // 等待期間（含等待佇列空間）以 keep_alive_while 定期更新本連線的 last_seen
    // （只更新 conn_id 相符的現役連線）。在放入佇列前逾時：這筆根本沒有進入佇列
    let durable = record.dedup.is_some();
    // 補送的資料（取樣時間比收到的時間早 BACKFILL_LAG_MS 以上）：保存後記入狀態表（「補送中」）
    let backfill_lag = if durable {
        DateTime::parse_from_rfc3339(&record.ts)
            .ok()
            .map(|ts| record.recv_at.timestamp_millis() - ts.timestamp_millis())
            .filter(|lag| *lag >= BACKFILL_LAG_MS)
    } else {
        None
    };
    let submit = keep_alive_while(registry, &agent_id, conn_id, revived, sink.submit(record));
    let mut pending = match timeout_at(deadline, submit).await {
        Ok(Ok(pending)) => pending,
        Ok(Err(reason)) => {
            stats.record(WriteOutcome::Unavailable);
            return StoreOutcome::Unavailable(reason);
        }
        Err(_) => {
            stats.record(WriteOutcome::Unavailable);
            return StoreOutcome::Unavailable(timed_out());
        }
    };
    let status = match timeout_at(
        deadline,
        keep_alive_while(registry, &agent_id, conn_id, revived, pending.wait()),
    )
    .await
    {
        Ok(status) => status,
        // writer 尚未認領：取消成功後保證不會寫入
        Err(_) if pending.cancel() => {
            stats.record(WriteOutcome::Unavailable);
            return StoreOutcome::Unavailable(timed_out());
        }
        // writer 已在寫入這筆：結果很快就會出來，多等一小段
        Err(_) => match timeout(
            write_timeout.min(CLAIMED_WRITE_GRACE),
            keep_alive_while(registry, &agent_id, conn_id, revived, pending.wait()),
        )
        .await
        {
            Ok(status) => status,
            Err(_) => {
                stats.record(WriteOutcome::Unknown);
                return StoreOutcome::Unknown(format!("{}，且寫入已開始、結果未知", timed_out()));
            }
        },
    };
    stats.record(match &status {
        WriteStatus::Written => WriteOutcome::Written,
        WriteStatus::Duplicate => WriteOutcome::Duplicate,
        WriteStatus::Failed(_) => WriteOutcome::Unavailable,
        WriteStatus::Unknown(_) => WriteOutcome::Unknown,
    });
    match status {
        WriteStatus::Written | WriteStatus::Duplicate => {
            if let Some(lag) = backfill_lag {
                registry.note_backfill(
                    &agent_id,
                    conn_id,
                    lag,
                    std::time::Instant::now(),
                    Utc::now(),
                );
            }
            // 寫入與此處之間若剛好被取代，資料已保存，只是不更新 last_report（新連線會有自己的 report）。
            // 重複的 report（at-least-once 的重送，先前已保存）不更新 last_report：它是較舊的資料。
            // durable report 可能是補送的舊資料，只在 ts 不早於目前的 last_report 時更新
            if status == WriteStatus::Written {
                if durable {
                    registry.set_last_report_unless_older(&agent_id, conn_id, last);
                } else {
                    registry.set_last_report(&agent_id, conn_id, last);
                }
                if let Some((hub, sample)) = export {
                    hub.accept(sample);
                }
            }
            let touch = registry.touch(&agent_id, conn_id, std::time::Instant::now(), Utc::now());
            if touch == Touch::Revived {
                revived.store(true, Ordering::Relaxed);
            }
            StoreOutcome::Stored(if revived.load(Ordering::Relaxed) {
                Touch::Revived
            } else {
                touch
            })
        }
        WriteStatus::Failed(reason) => StoreOutcome::Unavailable(reason),
        WriteStatus::Unknown(reason) => StoreOutcome::Unknown(reason),
    }
}

/// 執行 `fut` 直到完成；期間每 [`STORE_KEEPALIVE_INTERVAL`] 更新一次本連線的 last_seen，
/// 讓 agent 從離線恢復時設定 `revived`
async fn keep_alive_while<F: Future>(
    registry: &Registry,
    agent_id: &str,
    conn_id: u64,
    revived: &AtomicBool,
    fut: F,
) -> F::Output {
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval_at(
        Instant::now() + STORE_KEEPALIVE_INTERVAL,
        STORE_KEEPALIVE_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            out = &mut fut => return out,
            _ = ticker.tick() => {
                // 已被取代（Stale）時不更新任何狀態；恢復上線（Revived）交給呼叫端記錄
                if registry.touch(agent_id, conn_id, std::time::Instant::now(), Utc::now())
                    == Touch::Revived
                {
                    revived.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

/// hello 通過後的 agent 資訊
struct Agent {
    /// 正規化（小寫）後的 agent_id
    agent_id: String,
    hostname: String,
    agent_version: String,
    heartbeat_interval_ms: u64,
    hello_seq: u64,
    /// 是否已以 mTLS 驗證 agent_id 與客戶端憑證綁定（決定 report 限速的來源）
    authenticated: bool,
    /// 協商結果（版本與 report 格式）
    session: Session,
    /// v2 hello 的主機資訊（v1、或全部欄位都省略時為 `None`）
    host: Option<Arc<HostInfo>>,
    /// 雙方都宣告的 8443 註冊 feature（ENROLL.md 第 13 節）
    renew: bool,
    deregister: bool,
    /// hello 的 `intent` 為 `deregister`
    intent_deregister: bool,
    /// 雙方都宣告 `cluster`（v2）：`hello_ack` 帶 `server_node`，關機時送 `drain`
    cluster: bool,
}

/// 從 hello 取出、交給 [`Conn::accept_hello`] 的欄位
struct HelloFields {
    seq: u64,
    proto: Proto,
    agent_id: String,
    hostname: String,
    agent_version: String,
    heartbeat_interval_ms: u64,
    ext: HelloExt,
}

/// 依協商格式取出的 report 內容
enum ReportContent {
    /// v1 的三個欄位（`cpu_usage` 已轉成伺服器保存用的 f32）
    Legacy {
        cpu_usage: f32,
        total_memory: u64,
        used_memory: u64,
    },
    /// v2 的通用指標
    Metrics(Vec<Metric>),
}

impl ReportContent {
    /// 內容限制（違反時回 `invalid_report`）
    fn validate(&self) -> Result<(), String> {
        match self {
            ReportContent::Legacy {
                cpu_usage,
                total_memory,
                used_memory,
            } => validate_report(*cpu_usage, *total_memory, *used_memory),
            ReportContent::Metrics(metrics) => validate_metrics(metrics),
        }
    }

    /// debug 日誌用的摘要
    fn summary(&self) -> String {
        match self {
            ReportContent::Legacy {
                cpu_usage,
                total_memory,
                used_memory,
            } => format!("cpu={cpu_usage:.2}% mem={used_memory}/{total_memory} MB"),
            ReportContent::Metrics(metrics) => format!("{} 個指標", metrics.len()),
        }
    }

    fn into_body(self) -> RecordBody {
        match self {
            ReportContent::Legacy {
                cpu_usage,
                total_memory,
                used_memory,
            } => RecordBody::Legacy {
                cpu_usage,
                total_memory,
                used_memory,
            },
            ReportContent::Metrics(metrics) => RecordBody::Metrics { metrics },
        }
    }
}

/// 日誌中顯示主機指紋的前 19 個字元（`sha256:` 加 12 個十六進位字元）
fn short_fingerprint(fp: &str) -> String {
    let short: String = fp.chars().take(19).collect();
    if short.len() < fp.len() {
        format!("{short}…")
    } else {
        short
    }
}

/// [`Conn::accept_hello`] 的結果
enum HelloOutcome {
    /// 通過，可以註冊
    Accepted(Agent),
    /// hello 被拒（非 fatal 時繼續等待 hello）
    Rejected(Flow),
    /// 已送出 fatal 錯誤，連線結束
    Closed,
}

/// 確保連線結束時（含 panic 與被中止）以 conn_id 標記離線（R2-K20）
struct DisconnectGuard {
    registry: Arc<Registry>,
    agent_id: String,
    conn_id: u64,
    armed: bool,
}

impl DisconnectGuard {
    fn new(registry: Arc<Registry>, agent_id: String, conn_id: u64) -> Self {
        Self {
            registry,
            agent_id,
            conn_id,
            armed: true,
        }
    }

    /// 正常結束時呼叫；回傳 `true` 表示這次呼叫把 agent 從線上改為離線
    fn disconnect(mut self) -> bool {
        self.armed = false;
        self.registry
            .disconnect(&self.agent_id, self.conn_id, std::time::Instant::now())
    }
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        if self.armed {
            // conn_id 不符（從未註冊或已被取代）時不做任何事
            self.registry
                .disconnect(&self.agent_id, self.conn_id, std::time::Instant::now());
        }
    }
}

struct Conn {
    framed: Transport,
    conn_id: u64,
    peer: SocketAddr,
    write_timeout: Duration,
    shutdown: Arc<Shutdown>,
    tls: Arc<TlsState>,
    throttle: Arc<LogThrottle>,
    binding: BindingPolicy,
    /// 握手時客戶端出示的憑證鏈（[0] 為 end-entity）；沒有出示時為 None
    client_chain: Option<ClientChain>,
    /// 連續無效訊息（bad_request 與 unknown_type）的次數；收到合法訊息時歸零
    bad_requests: u32,
    /// report 的速率限制：本連線的 bucket（未啟用 mTLS 時與來源 IP 的 bucket 一起使用）
    reports: TokenBucket,
    /// 跨連線共用的 report 限速
    report_limiter: Arc<ReportLimiter>,
    /// 所有訊息的速率限制
    messages: TokenBucket,
    /// 註冊服務（8443 的 `renew`、`deregister`）
    enroll: Option<Arc<crate::enroll::EnrollService>>,
    /// 事件匯流排（身分驗證失敗、除役、狀態表淘汰；[`crate::bus`]）
    bus: Arc<crate::bus::EventBus>,
    /// 上一則接受（通過檢查、送去保存）的 `inventory` 的時間（每條連線最多每 60 秒一則）
    last_inventory: Option<Instant>,
    /// 資產的寫入佇列（SQLite 時；有值時宣告 `inventory`、`processes`）
    assets: Option<crate::store::AssetSink>,
    /// 個資政策（`[privacy]`）
    privacy: Arc<crate::privacy::PrivacyPolicy>,
    /// 要保存的來源位址（`privacy.store_peer_ip`；狀態表、清冊、report、資產）。日誌與限速仍用 `peer`
    stored_peer: String,
    /// `hello_ack.server_node`（協商了 `cluster` 時）
    node_id: Arc<str>,
}

/// 處理一條連線。握手失敗會在這裡（節流後）記錄並回傳 `Ok`；其他錯誤只影響此連線，由呼叫端記錄。
/// `slot` 為 accept 時取得的來源位址與「握手中」的名額（R2-K2、A7）：hello 通過後歸還握手中的名額，
/// 以 mTLS 驗證身分時另改依憑證身分計數（見 [`ConnectionSlot`]）；其餘在連線結束時釋放。
pub async fn handle_connection(
    ctx: Arc<ConnContext>,
    acceptor: TlsAcceptor,
    socket: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    mut slot: ConnectionSlot,
) -> Result<(), Error> {
    let shutdown = ctx.shutdown.clone();
    // into_fallible：握手失敗時 tokio-rustls 會先送出 TLS alert，再把底層 socket 交還
    let handshake = tokio::select! {
        biased;
        // 握手尚未完成，無法送 close_notify，直接放棄
        _ = shutdown.cancelled() => return Ok(()),
        // 握手時被拒的客戶端憑證以來源位址記錄（tls::rejections，設計包 §24 #3）
        res = timeout(
            ctx.timeouts.handshake(),
            crate::tls::HANDSHAKE_PEER.scope(peer, acceptor.accept(socket).into_fallible()),
        ) => res,
    };
    let tls_stream = match handshake {
        Ok(Ok(stream)) => stream,
        Ok(Err((e, mut tcp))) => {
            log_handshake_failure(&ctx.throttle, conn_id, peer, &e);
            // alert 已送出；送 FIN 後讀取丟棄對方已送出的資料再關閉，避免 RST 讓對方收不到 alert
            let _ = tcp.shutdown().await;
            let limit = drain_limit(
                shutdown.started_at(),
                std::time::Instant::now(),
                HANDSHAKE_DRAIN_TIMEOUT,
            );
            drain(&mut tcp, limit).await;
            return Ok(());
        }
        Err(_) => {
            throttled!(
                ctx.throttle,
                Category::Handshake,
                warn,
                "[#{}] {} TLS 握手未在 {} 秒內完成，關閉連線",
                conn_id,
                peer,
                ctx.timeouts.handshake().as_secs()
            );
            return Ok(());
        }
    };
    debug!("[#{}] {} TLS 握手完成", conn_id, peer);
    let client_chain: Option<ClientChain> = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .filter(|certs| !certs.is_empty())
        .map(|certs| certs.iter().map(|c| c.clone().into_owned()).collect());

    let mut conn = Conn {
        framed: Framed::new(tls_stream, NdjsonCodec::new()),
        conn_id,
        peer,
        write_timeout: ctx.timeouts.write(),
        shutdown,
        tls: ctx.tls.clone(),
        throttle: ctx.throttle.clone(),
        binding: ctx.binding,
        client_chain,
        bad_requests: 0,
        reports: ReportLimiter::connection_bucket(std::time::Instant::now()),
        report_limiter: ctx.report_limiter.clone(),
        messages: TokenBucket::new(MESSAGE_BURST, MESSAGES_PER_SEC, std::time::Instant::now()),
        enroll: ctx.enroll.clone(),
        bus: ctx.registry.bus(),
        last_inventory: None,
        assets: ctx.assets.clone(),
        privacy: ctx.privacy.clone(),
        stored_peer: ctx.privacy.peer(peer),
        node_id: ctx.cluster.node_id.clone(),
    };

    let Some(agent) = conn.await_hello(ctx.timeouts.hello()).await? else {
        return Ok(());
    };
    // 除役（ENROLL.md 第 13.2 節、第 18 節第 1 項）：已除役的主機只受理 intent: "deregister"
    if let Some(svc) = conn.enroll.clone()
        && agent.authenticated
    {
        if agent.intent_deregister {
            return conn
                .run_deregister(&svc, &agent, ctx.timeouts.hello())
                .await;
        }
        if svc.is_decommissioned(&agent.agent_id) {
            throttled!(
                ctx.throttle,
                (Category::AuthFailed, peer.ip()),
                info,
                "[#{}] agent {} 已除役，拒絕 hello（只受理 intent: \"deregister\"）",
                conn_id,
                agent.agent_id
            );
            conn.reply(ServerMsg::error(
                Some(agent.hello_seq),
                ErrorCode::CertificateRejected,
                MSG_CERTIFICATE_REJECTED,
            ))
            .await?;
            return Ok(());
        }
        // 外部 CA 簽發、匯入簽發紀錄後在 Kairos 撤銷的憑證：Kairos 不產生它們的 CRL，hello 時拒絕
        if let Some(serial) = conn.client_serial()
            && svc.is_revoked_external(&serial)
        {
            throttled!(
                ctx.throttle,
                (Category::CertificateRevoked, peer.ip()),
                warn,
                "[#{}] 警告 KRS-W1105：agent {} 的憑證 {} 已在 Kairos 撤銷（外部 CA 簽發、匯入的憑證），拒絕 hello",
                conn_id,
                agent.agent_id,
                serial
            );
            conn.reply(ServerMsg::error(
                Some(agent.hello_seq),
                ErrorCode::CertificateRejected,
                MSG_CERTIFICATE_REJECTED,
            ))
            .await?;
            return Ok(());
        }
        if let Some(serial) = conn.client_serial() {
            crate::enroll::agent::on_hello(&svc, &agent.agent_id, &serial);
        }
    }
    // hello 已通過：不再佔用「握手中」的名額；mTLS 時改依憑證身分計數（A7）
    slot.hello_passed(agent.authenticated.then_some(agent.agent_id.as_str()));

    // 註冊後不論結果如何（含 panic、被中止）都要以 conn_id 比對並標記離線
    let guard = DisconnectGuard::new(ctx.registry.clone(), agent.agent_id.clone(), conn_id);
    let result = conn.run_registered(&ctx, &agent).await;
    if guard.disconnect() {
        throttled!(
            ctx.throttle,
            Key::agent_event(
                Category::AgentOffline,
                peer.ip(),
                &agent.agent_id,
                agent.authenticated
            ),
            info,
            "[#{}] agent {}（{}）連線關閉，標記為離線",
            conn_id,
            agent.agent_id,
            agent.hostname.escape_debug()
        );
    } else {
        debug!("[#{}] agent {} 的連線結束", conn_id, agent.agent_id);
    }
    result
}

/// 握手失敗的日誌：撤銷與 CRL 狀態的錯誤與一般握手失敗分開節流，避免被掃描流量掩蓋
fn log_handshake_failure(throttle: &LogThrottle, conn_id: u64, peer: SocketAddr, e: &Error) {
    if is_peer_disconnect(e) {
        debug!("[#{}] {} 在 TLS 握手期間中斷連線: {}", conn_id, peer, e);
        return;
    }
    match revocation_failure(e) {
        Some(RevocationFailure::Revoked) => throttled!(
            throttle,
            (Category::CertificateRevoked, peer.ip()),
            warn,
            "[#{}] {} 出示的客戶端憑證已被撤銷，TLS 握手失敗: {}",
            conn_id,
            peer,
            e
        ),
        Some(RevocationFailure::StatusUnavailable) => throttled!(
            throttle,
            Category::RevocationStatus,
            error,
            "[#{}] {} TLS 握手失敗：無法判定客戶端憑證的撤銷狀態，CRL 可能已過期或缺少該層 CA 的 CRL，請更新 auth.crl: {}",
            conn_id,
            peer,
            e
        ),
        // 無客戶端憑證、未知 CA、過期、非 TLS 流量等；可能被掃描大量觸發，節流記錄
        None => throttled!(
            throttle,
            Category::Handshake,
            warn,
            "[#{}] {} TLS 握手失敗: {}",
            conn_id,
            peer,
            e
        ),
    }
}

impl Conn {
    /// 這條連線的客戶端憑證序號（大寫十六進位）
    fn client_serial(&self) -> Option<String> {
        self.client_chain
            .as_deref()
            .and_then(|c| c.first())
            .and_then(|c| crate::tls::cert_serial_hex(c.as_ref()))
            .map(|s| s.to_ascii_uppercase())
    }

    /// `hello_ack`：v2 時帶 features 與 `server_time`；協商了 `renew` 時另帶 `expired_renew_grace_days` 與
    /// 選用的 `renew_now`（PROTOCOL.md「憑證輪替與除役」）。`renew`、`deregister` 只在 client 也宣告時列出：
    /// 沒有宣告的 client（0.4 以前的 Pulse）收到的 `hello_ack` 與 0.4 的 Kairos 完全相同
    async fn send_hello_ack(&mut self, agent: &Agent, offline_ms: u64) -> Result<(), Error> {
        let (features, server_time) = if agent.session.is_v2() {
            let mut f: Vec<String> = server_features(self.assets.is_some())
                .iter()
                .map(|f| f.to_string())
                .collect();
            if agent.renew {
                f.push(FEATURE_RENEW.to_string());
            }
            if agent.deregister {
                f.push(FEATURE_DEREGISTER.to_string());
            }
            if agent.cluster {
                f.push(FEATURE_CLUSTER.to_string());
            }
            (Some(f), Some(fmt_time(Utc::now())))
        } else {
            // v1：hello_ack 與 v1 Kairos 完全相同
            (None, None)
        };
        let renew = match (agent.renew, self.enroll.clone()) {
            (true, Some(svc)) => {
                let cert = self
                    .client_chain
                    .as_deref()
                    .and_then(|c| c.first())
                    .map(|c| c.as_ref().to_vec());
                tokio::task::spawn_blocking(move || {
                    crate::enroll::agent::hello_ack_renew(&svc, cert.as_deref())
                })
                .await
                .unwrap_or_default()
            }
            _ => crate::enroll::agent::HelloAckRenew::default(),
        };
        let msg = ServerMsg::HelloAck {
            seq: agent.hello_seq,
            proto: agent.session.proto,
            offline_after_ms: offline_ms,
            features,
            server_time,
            expired_renew_grace_days: renew.expired_renew_grace_days,
            renew_now: renew.renew_now,
            // 叢集模式（cluster_id）留待之後的版本；單機的 Kairos 只帶節點名稱
            server_node: agent.cluster.then(|| self.node_id.to_string()),
            cluster_id: None,
        };
        self.send(&msg).await
    }

    /// 優雅關機：協商了 `cluster` 的連線先送 `drain`（PROTOCOL.md「叢集與排空」），讓 agent 在
    /// `reconnect_within_ms` 內分散改連其他節點；之後照常送 close_notify。寫出不超過連線收尾期限，
    /// 失敗時直接關閉（agent 視為一般斷線）。回傳是否送出
    async fn send_drain_for_shutdown(&mut self, reconnect_within_ms: u32) -> bool {
        let remaining = match self.shutdown.started_at() {
            Some(t0) => {
                (t0 + CONNECTIONS_DEADLINE).saturating_duration_since(std::time::Instant::now())
            }
            None => self.write_timeout,
        };
        let limit = self.write_timeout.min(remaining);
        if limit.is_zero() {
            return false;
        }
        let msg = ServerMsg::drain(DRAIN_REASON_NODE_DRAINING, reconnect_within_ms);
        let Ok(line) = msg.to_line() else {
            return false;
        };
        match timeout(limit, self.framed.send(line)).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                debug!("[#{}] {} 送出 drain 失敗：{}", self.conn_id, self.peer, e);
                false
            }
            Err(_) => {
                debug!("[#{}] {} 送出 drain 逾時", self.conn_id, self.peer);
                false
            }
        }
    }

    /// `renew_request`：正常輪替（PROTOCOL.md「憑證輪替與除役」）。錯誤都不是 fatal
    async fn handle_renew(
        &mut self,
        svc: &Arc<crate::enroll::EnrollService>,
        agent: &Agent,
        req: kairos_proto::RenewRequest,
    ) -> Result<Handled, Error> {
        self.bad_requests = 0;
        let seq = req.seq;
        let reply = match crate::enroll::agent::renew(svc, &agent.agent_id, req).await {
            Ok(result) => ServerMsg::RenewResult(Box::new(result)),
            Err(e) => e.to_msg(Some(seq)),
        };
        self.send(&reply).await?;
        Ok(Handled::alive(Flow::Continue))
    }

    /// `intent: "deregister"` 的連線：送出 `hello_ack`（不註冊、不算上線、不取代既有連線），等待一則
    /// `deregister` 並回 `deregister_ack` 後關閉（PROTOCOL.md「憑證輪替與除役」）
    async fn run_deregister(
        &mut self,
        svc: &Arc<crate::enroll::EnrollService>,
        agent: &Agent,
        wait: Duration,
    ) -> Result<(), Error> {
        if !agent.deregister {
            self.reply(ServerMsg::error_with_fatal(
                Some(agent.hello_seq),
                ErrorCode::BadRequest,
                "intent deregister requires the deregister feature",
                true,
            ))
            .await?;
            return Ok(());
        }
        let offline_ms = offline_after_ms(agent.heartbeat_interval_ms);
        self.send_hello_ack(agent, offline_ms).await?;
        let deadline = deadline_after(wait);
        loop {
            let frame = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => return self.close_for_shutdown().await,
                r = timeout_at(deadline, self.framed.next()) => match r {
                    Err(_) | Ok(None) => return self.close().await,
                    Ok(Some(Err(e))) => return Err(e),
                    Ok(Some(Ok(f))) => f,
                },
            };
            let msg = match classify(frame) {
                Incoming::Msg(ClientMsg::Deregister(msg)) => msg,
                Incoming::Msg(ClientMsg::Heartbeat { .. }) => continue,
                Incoming::InvalidLifecycle { kind, message } if kind == "deregister" => {
                    self.reply(ServerMsg::error_with_fatal(
                        None,
                        ErrorCode::BadRequest,
                        format!("invalid deregister: {message}"),
                        true,
                    ))
                    .await?;
                    return Ok(());
                }
                _ => {
                    self.reply(ServerMsg::error_with_fatal(
                        None,
                        ErrorCode::BadRequest,
                        "only deregister is accepted on this connection",
                        true,
                    ))
                    .await?;
                    return Ok(());
                }
            };
            if normalize_agent_id(&msg.agent_id) != agent.agent_id {
                self.reply(ServerMsg::error(
                    Some(msg.seq),
                    ErrorCode::AuthFailed,
                    "agent_id does not match the client certificate",
                ))
                .await?;
                return Ok(());
            }
            let seq = msg.seq;
            let serial = self.client_serial();
            let peer = self.peer.ip().to_string();
            let initiated_by = Some(msg.initiated_by.clone());
            let reply =
                match crate::enroll::agent::deregister(svc, &agent.agent_id, serial, &peer, *msg)
                    .await
                {
                    Ok(ack) => {
                        self.bus.publish(crate::bus::EventKind::HostDecommissioned {
                            agent_id: agent.agent_id.clone(),
                            ip: Some(self.peer.ip().to_canonical()),
                            initiated_by,
                            already: ack.already_decommissioned,
                        });
                        ServerMsg::DeregisterAck(Box::new(ack))
                    }
                    Err(e) => e.to_msg(Some(seq)),
                };
            self.send(&reply).await?;
            return self.close().await;
        }
    }

    async fn send(&mut self, msg: &ServerMsg) -> Result<(), Error> {
        let line = msg.to_line()?;
        with_timeout(self.write_timeout, "寫出訊息", self.framed.send(line)).await
    }

    /// 送出 close_notify 並關閉寫入方向，接著讀取並丟棄對方剩餘的資料，
    /// 直到對方關閉或超過時限，避免 RST 讓對方收不到最後的訊息。
    /// 平常最多讀 [`DRAIN_TIMEOUT`]；關機期間最多 200 ms，且送出與讀取都不超過連線收尾期限，
    /// 期限已過時直接放棄（連線會被釋放，對方視為斷線）。
    async fn close(&mut self) -> Result<(), Error> {
        let close_timeout = match self.shutdown.started_at() {
            Some(t0) => self.write_timeout.min(
                (t0 + CONNECTIONS_DEADLINE).saturating_duration_since(std::time::Instant::now()),
            ),
            None => self.write_timeout,
        };
        if close_timeout.is_zero() {
            debug!(
                "[#{}] {} 關機收尾期限已過，不送 close_notify 直接釋放連線",
                self.conn_id, self.peer
            );
            return Ok(());
        }
        with_timeout(close_timeout, "關閉連線", self.framed.close()).await?;
        let limit = drain_limit(
            self.shutdown.started_at(),
            std::time::Instant::now(),
            DRAIN_TIMEOUT,
        );
        match drain(self.framed.get_mut(), limit).await {
            Some(n) if n > 0 => trace!("[#{}] 關閉前丟棄 {} bytes", self.conn_id, n),
            Some(_) => {}
            None => debug!(
                "[#{}] {} 在 {} ms 內未關閉連線，直接釋放",
                self.conn_id,
                self.peer,
                limit.as_millis()
            ),
        }
        Ok(())
    }

    /// 送出回應；若為 fatal 錯誤則接著關閉連線
    async fn reply(&mut self, msg: ServerMsg) -> Result<Flow, Error> {
        self.send(&msg).await?;
        if msg.is_fatal() {
            self.close().await?;
            Ok(Flow::Close)
        } else {
            Ok(Flow::Continue)
        }
    }

    /// 回覆無效訊息（`bad_request` 或 `unknown_type`）。兩者合計連續達上限（中間沒有合法訊息）時，
    /// 該則改為 fatal 並關閉連線，避免交錯送出兩種無效訊息來繞過上限。
    async fn reply_invalid(
        &mut self,
        code: ErrorCode,
        seq: Option<u64>,
        message: String,
    ) -> Result<Flow, Error> {
        self.bad_requests += 1;
        let fatal = self.bad_requests >= MAX_CONSECUTIVE_BAD_REQUESTS;
        if fatal {
            throttled!(
                self.throttle,
                Category::ProtocolViolation,
                warn,
                "[#{}] {} 連續 {} 則無效訊息，關閉連線（最後一則: {}）",
                self.conn_id,
                self.peer,
                self.bad_requests,
                message.escape_debug()
            );
        } else {
            debug!(
                "[#{}] {} 送出無效訊息（{}）: {}",
                self.conn_id,
                self.peer,
                code.as_str(),
                message.escape_debug()
            );
        }
        self.reply(ServerMsg::error_with_fatal(seq, code, message, fatal))
            .await
    }

    /// `seq` 為能從無效的 hello 取得的 seq（F5），其他情況為 `None`
    async fn reply_bad_request(
        &mut self,
        seq: Option<u64>,
        message: String,
    ) -> Result<Flow, Error> {
        self.reply_invalid(ErrorCode::BadRequest, seq, message)
            .await
    }

    async fn reply_unknown_type(&mut self) -> Result<Flow, Error> {
        self.reply_invalid(
            ErrorCode::UnknownType,
            None,
            "unknown message type".to_string(),
        )
        .await
    }

    async fn reply_too_long(&mut self) -> Result<Flow, Error> {
        throttled!(
            self.throttle,
            Category::ProtocolViolation,
            warn,
            "[#{}] {} 送出超過 {} bytes 的單行，關閉連線",
            self.conn_id,
            self.peer,
            MAX_LINE_LENGTH
        );
        self.reply(ServerMsg::error(
            None,
            ErrorCode::TooLong,
            format!("line exceeds {} bytes", MAX_LINE_LENGTH),
        ))
        .await
    }

    /// 已註冊 agent 的事件（上線、離線、被取代等）的節流 key：通過 mTLS 驗證時以 agent_id 節流（F1）
    fn agent_key(&self, category: Category, agent: &Agent) -> Key {
        Key::agent_event(
            category,
            self.peer.ip(),
            &agent.agent_id,
            agent.authenticated,
        )
    }

    async fn reply_superseded(&mut self, agent: &Agent) -> Result<Flow, Error> {
        throttled!(
            self.throttle,
            self.agent_key(Category::AgentOffline, agent),
            info,
            "[#{}] agent {} 已由新連線取代，關閉舊連線",
            self.conn_id,
            agent.agent_id
        );
        self.reply(ServerMsg::error(
            None,
            ErrorCode::Superseded,
            "superseded by a newer connection",
        ))
        .await
    }

    /// 啟用 mTLS 時（依目前的 TLS 設定）在 register 之前檢查客戶端憑證，因此冒用者無法踢掉既有連線：
    /// 1. 以目前的驗證器重新驗證憑證鏈（握手後才撤銷、過期，或握手後重載了 CA/CRL）；
    ///    不通過回 `certificate_rejected`；
    /// 2. hello 的 agent_id 必須與憑證的 SAN 相符（預設憑證只能有一個 DNS SAN）；不符回 `auth_failed`。
    ///
    /// 兩者都是 fatal，關閉連線並回傳 `None`；通過時回傳是否已以 mTLS 驗證。
    async fn check_identity(&mut self, seq: u64, agent_id: &str) -> Result<Option<bool>, Error> {
        let material = self.tls.current();
        let Some(verifier) = &material.verifier else {
            return Ok(Some(false));
        };
        if let Err(reason) = verify_chain(
            verifier.as_ref(),
            self.client_chain.as_deref(),
            UnixTime::now(),
        ) {
            throttled!(
                self.throttle,
                (Category::AuthFailed, self.peer.ip()),
                warn,
                "[#{}] {} 的客戶端憑證在 hello 時已不再有效（agent_id \"{}\"），關閉連線: {}",
                self.conn_id,
                self.peer,
                agent_id.escape_debug(),
                reason
            );
            // CRL 過期或無法判定撤銷狀態時，message 為 `revocation status unavailable`（R2-K26）
            self.reply(ServerMsg::error(
                Some(seq),
                ErrorCode::CertificateRejected,
                reason.client_message(),
            ))
            .await?;
            return Ok(None);
        }
        let result = match self.client_chain.as_deref().and_then(|c| c.first()) {
            Some(cert) => cert_matches_agent_id(cert, agent_id, self.binding),
            None => Err(BindingError::Mismatch("沒有客戶端憑證".to_string())),
        };
        match result {
            Ok(()) => Ok(Some(true)),
            Err(reason) => {
                throttled!(
                    self.throttle,
                    (Category::AuthFailed, self.peer.ip()),
                    warn,
                    "[#{}] {} 的 hello agent_id \"{}\" 未通過客戶端憑證綁定，關閉連線: {}",
                    self.conn_id,
                    self.peer,
                    agent_id.escape_debug(),
                    reason
                );
                self.bus.publish(crate::bus::EventKind::AuthFailed {
                    agent_id: agent_id.to_string(),
                    ip: self.peer.ip().to_canonical(),
                });
                self.reply(ServerMsg::error(
                    Some(seq),
                    ErrorCode::AuthFailed,
                    reason.client_message(),
                ))
                .await?;
                Ok(None)
            }
        }
    }

    /// 訊息速率上限：超過時延後處理（期間不讀取，TCP 背壓會讓送出端慢下來；R2-K12）
    async fn pace_message(&mut self) {
        let wait = self
            .messages
            .acquire(std::time::Instant::now())
            .min(MAX_MESSAGE_PACING_DELAY);
        if wait.is_zero() {
            return;
        }
        throttled!(
            self.throttle,
            (Category::MessageRate, self.peer.ip()),
            warn,
            "[#{}] {} 送出訊息超過每秒 {} 則（可連續 {} 則），延後處理",
            self.conn_id,
            self.peer,
            MESSAGES_PER_SEC,
            MESSAGE_BURST
        );
        tokio::time::sleep(wait).await;
    }

    /// 伺服器關機：送出 close_notify 後結束
    async fn close_for_shutdown(&mut self) -> Result<(), Error> {
        debug!("[#{}] {} 伺服器關機，關閉連線", self.conn_id, self.peer);
        self.close().await
    }

    /// 握手後等待 hello。回傳 `None` 表示連線已結束（逾時、對方關閉、關機或 fatal 錯誤）。
    async fn await_hello(&mut self, hello_timeout: Duration) -> Result<Option<Agent>, Error> {
        // 以固定期限計算，無效訊息不會延長等待時間
        let deadline = deadline_after(hello_timeout);
        loop {
            let res = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => {
                    self.close_for_shutdown().await?;
                    return Ok(None);
                }
                res = timeout_at(deadline, self.framed.next()) => res,
            };
            let frame = match res {
                Err(_) => {
                    throttled!(
                        self.throttle,
                        Category::HelloTimeout,
                        info,
                        "[#{}] {} 在 {} 秒內未送出 hello，關閉連線",
                        self.conn_id,
                        self.peer,
                        hello_timeout.as_secs()
                    );
                    self.close().await?;
                    return Ok(None);
                }
                Ok(None) => {
                    debug!("[#{}] {} 在送出 hello 前關閉連線", self.conn_id, self.peer);
                    return Ok(None);
                }
                Ok(Some(Err(e))) => return Err(e),
                Ok(Some(Ok(frame))) => frame,
            };
            self.pace_message().await;

            let flow = match classify(frame) {
                Incoming::TooLong => self.reply_too_long().await?,
                Incoming::BadRequest { message, seq } => {
                    self.reply_bad_request(seq, message).await?
                }
                Incoming::Msg(ClientMsg::Hello {
                    seq,
                    proto,
                    agent_id,
                    hostname,
                    agent_version,
                    heartbeat_interval_ms,
                    ext,
                    ..
                }) => {
                    let hello = HelloFields {
                        seq,
                        proto,
                        agent_id,
                        hostname,
                        agent_version,
                        heartbeat_interval_ms,
                        ext: *ext,
                    };
                    match self.accept_hello(hello).await? {
                        HelloOutcome::Accepted(agent) => {
                            self.bad_requests = 0;
                            return Ok(Some(agent));
                        }
                        HelloOutcome::Rejected(flow) => flow,
                        HelloOutcome::Closed => return Ok(None),
                    }
                }
                Incoming::Msg(ClientMsg::Heartbeat { seq, .. } | ClientMsg::Report { seq, .. }) => {
                    throttled!(
                        self.throttle,
                        Category::ProtocolViolation,
                        warn,
                        "[#{}] {} 未先送出 hello，關閉連線",
                        self.conn_id,
                        self.peer
                    );
                    self.reply(ServerMsg::error(
                        Some(seq),
                        ErrorCode::HelloRequired,
                        "hello required before other messages",
                    ))
                    .await?
                }
                // 不重置連續無效訊息的計數（避免與 bad_request 交錯送出來繞過上限）。
                // hello 之前的 renew_request／deregister 與舊版相同，視為不認得的 type
                Incoming::Msg(
                    ClientMsg::Unknown
                    | ClientMsg::RenewRequest(_)
                    | ClientMsg::Deregister(_)
                    | ClientMsg::Inventory(_),
                )
                | Incoming::InvalidLifecycle { .. } => self.reply_unknown_type().await?,
            };
            if let Flow::Close = flow {
                return Ok(None);
            }
        }
    }

    /// 協商版本並驗證 hello 的欄位（A3）。
    ///
    /// 順序：`proto` 不合法或沒有共同版本 → fatal 的 `unsupported_version`；版本範圍或 features
    /// 型別錯誤 → `bad_request`；v1 欄位（agent_id、hostname、agent_version）→ `bad_request`；
    /// 協商為 v2 時再檢查 features 與主機資訊 → `bad_request`。協商為 v1 時主機資訊一律忽略，
    /// 與 v1 Kairos 相同。`bad_request` 都帶回 hello 的 seq（F5），並計入連續無效訊息。
    async fn accept_hello(&mut self, hello: HelloFields) -> Result<HelloOutcome, Error> {
        let HelloFields {
            seq,
            proto,
            agent_id,
            hostname,
            agent_version,
            heartbeat_interval_ms,
            ext,
        } = hello;
        let range = match client_proto_range(&proto, &ext.offer) {
            Ok(range) => Some(range),
            Err(ProtoRangeError::InvalidProto) => None,
            Err(ProtoRangeError::BadOffer(message)) => {
                return Ok(HelloOutcome::Rejected(
                    self.reply_bad_request(Some(seq), message).await?,
                ));
            }
        };
        let Some(version) = range.and_then(|range| negotiate(range, SUPPORTED_PROTO)) else {
            let offered = match range {
                Some(r) if r.min != r.max => format!("{}（範圍 {}～{}）", proto, r.min, r.max),
                _ => proto.to_string(),
            };
            throttled!(
                self.throttle,
                Category::ProtocolViolation,
                warn,
                "[#{}] {} 的協定版本 {} 不受支援（agent_id={:?}，本伺服器支援 {}～{}），關閉連線",
                self.conn_id,
                self.peer,
                offered,
                agent_id,
                SUPPORTED_PROTO.min,
                SUPPORTED_PROTO.max
            );
            self.reply(unsupported_version_error(Some(seq), SUPPORTED_PROTO))
                .await?;
            return Ok(HelloOutcome::Closed);
        };
        // 欄位不合規定：計入連續無效訊息，繼續等待 hello；回應帶回 hello 的 seq（F5）
        if let Err(message) = validate_hello(&agent_id, &hostname, &agent_version) {
            return Ok(HelloOutcome::Rejected(
                self.reply_bad_request(Some(seq), message).await?,
            ));
        }
        let client_features = match &ext.offer {
            Ok(offer) => offer.features.clone().unwrap_or_default(),
            Err(_) => Vec::new(), // client_proto_range 已拒絕
        };
        let session = Session::new(
            version,
            &server_features(self.assets.is_some()),
            &client_features,
        );
        let host = if session.is_v2() {
            let checked = validate_features(&client_features).and_then(|()| {
                let host = ext.host.map_err(|e| format!("invalid message: {e}"))?;
                validate_host_info(&host)?;
                Ok(host)
            });
            match checked {
                Ok(host) => (host != HostInfo::default()).then(|| Arc::new(host)),
                Err(message) => {
                    return Ok(HelloOutcome::Rejected(
                        self.reply_bad_request(Some(seq), message).await?,
                    ));
                }
            }
        } else {
            None
        };
        // UUID 不分大小寫：之後的驗證、註冊、日誌與寫檔一律使用小寫
        let agent_id = normalize_agent_id(&agent_id);
        let Some(authenticated) = self.check_identity(seq, &agent_id).await? else {
            return Ok(HelloOutcome::Closed);
        };
        let (renew, deregister) = lifecycle_features(
            authenticated,
            session.is_v2(),
            self.enroll.is_some(),
            &client_features,
        );
        // 個資政策：之後的狀態表、清冊、report、事件都只看到轉換後的主機名稱與顯示名稱
        let hostname = self.privacy.hostname(&hostname);
        let host = host.map(|h| Arc::new(self.privacy.host_info((*h).clone())));
        Ok(HelloOutcome::Accepted(Agent {
            agent_id,
            hostname,
            agent_version,
            heartbeat_interval_ms,
            hello_seq: seq,
            authenticated,
            session,
            host,
            renew,
            deregister,
            intent_deregister: ext.intent.as_deref() == Some(INTENT_DEREGISTER),
            cluster: session.is_v2() && client_features.iter().any(|f| f == FEATURE_CLUSTER),
        }))
    }

    /// 註冊 agent 並處理後續訊息，直到連線結束
    async fn run_registered(&mut self, ctx: &ConnContext, agent: &Agent) -> Result<(), Error> {
        let clamped = clamp_heartbeat_interval_ms(agent.heartbeat_interval_ms);
        if clamped != agent.heartbeat_interval_ms {
            throttled!(
                self.throttle,
                (Category::HeartbeatClamped, self.peer.ip()),
                warn,
                "[#{}] agent {} 宣告的 heartbeat_interval_ms={} 超出 1000..=60000，以 {} 計算離線門檻",
                self.conn_id,
                agent.agent_id,
                agent.heartbeat_interval_ms,
                clamped
            );
        }
        let offline_ms = offline_after_ms(agent.heartbeat_interval_ms);
        let offline_after = Duration::from_millis(offline_ms);

        let registered = ctx.registry.register(
            self.conn_id,
            HelloInfo {
                agent_id: agent.agent_id.clone(),
                hostname: agent.hostname.clone(),
                agent_version: agent.agent_version.clone(),
                proto: agent.session.proto,
                host: agent.host.clone(),
                peer: self.stored_peer.clone(),
                offline_after,
                client_chain: self.client_chain.clone(),
            },
            std::time::Instant::now(),
            Utc::now(),
        );
        let registration = match registered {
            Ok(registration) => registration,
            Err(full) => {
                throttled!(
                    self.throttle,
                    Category::RegistryFull,
                    warn,
                    "[#{}] 狀態表已達上限 {}（limits.max_agents）且沒有可淘汰的離線 agent，拒絕 {} 的新 agent {}",
                    self.conn_id,
                    full.max_agents,
                    self.peer,
                    agent.agent_id
                );
                // 防禦性：設定保證走不到（見 RegistryFull）。沒有專用的錯誤碼；
                // 以 fatal 的 bad_request 拒絕（Pulse 在 hello_ack 前收到錯誤會退避重連）
                self.reply(ServerMsg::error_with_fatal(
                    Some(agent.hello_seq),
                    ErrorCode::BadRequest,
                    "server agent registry is full",
                    true,
                ))
                .await?;
                return Ok(());
            }
        };
        // 主控台 API 的「支援補送」與主機詳情（A5）、資產與行程的支援（METRICS.md）
        ctx.registry
            .set_durable(&agent.agent_id, self.conn_id, agent.session.durable);
        ctx.registry.set_features(
            &agent.agent_id,
            self.conn_id,
            agent.session.inventory,
            agent.session.processes,
        );
        if let Some(evicted) = &registration.evicted {
            throttled!(
                self.throttle,
                Category::RegistryEvicted,
                info,
                "狀態表已達上限（limits.max_agents），移除離線最久的 agent {}（{}，已離線 {:.1} 小時）",
                evicted.agent_id,
                evicted.hostname.escape_debug(),
                evicted.offline_for.as_secs_f64() / 3600.0
            );
            self.bus.publish(crate::bus::EventKind::RegistryEvicted {
                agent_id: evicted.agent_id.clone(),
                hostname: evicted.hostname.clone(),
            });
        }
        let state = match registration.kind {
            RegisterKind::New => "上線".to_string(),
            RegisterKind::Reconnected => "重新上線".to_string(),
            RegisterKind::Superseded { previous_conn_id } => {
                format!("重新連線，取代舊連線 #{}", previous_conn_id)
            }
        };
        // hostname / agent_version 由對方提供：已在 hello 驗證拒絕控制與格式字元，日誌輸出時再跳脫一次
        throttled!(
            self.throttle,
            self.agent_key(Category::AgentOnline, agent),
            info,
            "[#{}] agent {}（{}，v{}，{}）{}，離線門檻 {} ms，協定 v{}{}",
            self.conn_id,
            agent.agent_id,
            agent.hostname.escape_debug(),
            agent.agent_version.escape_debug(),
            self.peer,
            state,
            offline_ms,
            agent.session.proto,
            if agent.session.durable {
                "（durable：report 可補送、去重）"
            } else {
                ""
            }
        );
        if let Some(previous) = &registration.previous_fingerprint {
            throttled!(
                self.throttle,
                self.agent_key(Category::HostFingerprintChanged, agent),
                warn,
                "[#{}] agent {} 的主機指紋與上次不同（{} → {}）：可能有多台主機共用同一份 agent.id（例如由映像檔複製的 VM），或主機已重灌",
                self.conn_id,
                agent.agent_id,
                short_fingerprint(previous),
                agent
                    .host
                    .as_ref()
                    .and_then(|h| h.host_fingerprint.as_deref())
                    .map_or_else(String::new, short_fingerprint)
            );
        }

        self.send_hello_ack(agent, offline_ms).await?;

        let mut kick: oneshot::Receiver<KickReason> = registration.kick;
        let mut kick_armed = true;
        // 只有合法訊息會延後此期限；壞行與未知 type 不算
        let mut idle_deadline = deadline_after(offline_after);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => {
                    if agent.cluster
                        && self
                            .send_drain_for_shutdown(ctx.cluster.drain_window(&ctx.registry))
                            .await
                    {
                        ctx.cluster
                            .drains_sent
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        debug!("[#{}] 已對 agent {} 送出 drain", self.conn_id, agent.agent_id);
                    }
                    self.close_for_shutdown().await?;
                    return Ok(());
                }
                res = &mut kick, if kick_armed => match res {
                    Ok(KickReason::Superseded) => {
                        self.reply_superseded(agent).await?;
                        return Ok(());
                    }
                    Ok(KickReason::CertificateRejected(reason)) => {
                        // 每條連線最多一次，數量受連線數限制；reload 也會記錄一行 warn
                        debug!(
                            "[#{}] agent {} 的客戶端憑證已被撤銷、不再受信任或無法判定撤銷狀態（{}），關閉連線",
                            self.conn_id, agent.agent_id, reason
                        );
                        self.reply(ServerMsg::error(
                            None,
                            ErrorCode::CertificateRejected,
                            reason.client_message(),
                        ))
                        .await?;
                        return Ok(());
                    }
                    // sender 被釋放但沒有送出通知：之後不可能再被踢，停止監聽
                    Err(_) => kick_armed = false,
                },
                res = timeout_at(idle_deadline, self.framed.next()) => {
                    let frame = match res {
                        Err(_) => {
                            throttled!(
                                self.throttle,
                                self.agent_key(Category::AgentOffline, agent),
                                info,
                                "[#{}] agent {} 超過 {} ms 沒有合法訊息，關閉連線",
                                self.conn_id,
                                agent.agent_id,
                                offline_ms
                            );
                            self.close().await?;
                            return Ok(());
                        }
                        Ok(None) => return Ok(()),
                        Ok(Some(Err(e))) => return Err(e),
                        Ok(Some(Ok(frame))) => frame,
                    };
                    // 讀到這一行的時間：之後的限速等待不影響 report 的 recv_at（R2-K16）
                    let received_at = Utc::now();
                    self.pace_message().await;
                    let handled = self.handle_registered(ctx, agent, frame, received_at).await?;
                    if let Flow::Close = handled.flow {
                        return Ok(());
                    }
                    if handled.alive {
                        idle_deadline = deadline_after(offline_after);
                    }
                }
            }
        }
    }

    /// 處理 registry 更新結果。回傳 `false` 表示本連線已被取代。
    fn on_touch(&self, agent: &Agent, touch: Touch) -> bool {
        match touch {
            Touch::Updated => true,
            Touch::Revived => {
                throttled!(
                    self.throttle,
                    self.agent_key(Category::AgentOnline, agent),
                    info,
                    "[#{}] agent {}（{}）恢復上線",
                    self.conn_id,
                    agent.agent_id,
                    agent.hostname.escape_debug()
                );
                true
            }
            Touch::Stale => false,
        }
    }

    /// 更新 last_seen。回傳 `false` 表示本連線已被取代。
    fn touch(&self, ctx: &ConnContext, agent: &Agent) -> bool {
        let touch = ctx.registry.touch(
            &agent.agent_id,
            self.conn_id,
            std::time::Instant::now(),
            Utc::now(),
        );
        self.on_touch(agent, touch)
    }

    /// report 速率限制（跨連線；R2-K3）：超過時延後處理（期間不讀取，TCP 背壓會讓送出端慢下來）。
    /// 需要延後超過 [`MAX_REPORT_PACING_DELAY`]（同一來源有多條連線同時送出）時回傳 `false`，
    /// 這筆不處理，回 `storage_unavailable`（確定未保存）。
    async fn pace_report(&mut self, agent: &Agent) -> bool {
        let now = std::time::Instant::now();
        let pace = if agent.authenticated {
            // mTLS：agent_id 與憑證綁定，以 agent_id 為來源
            self.report_limiter.acquire(
                ReportSource::Agent(agent.agent_id.clone()),
                None,
                now,
                MAX_REPORT_PACING_DELAY,
            )
        } else {
            // agent_id 可任意宣告：以來源 IP（IPv6 /64）為來源，並限制本連線
            self.report_limiter.acquire(
                ReportSource::Ip(source_key(self.peer.ip())),
                Some(&mut self.reports),
                now,
                MAX_REPORT_PACING_DELAY,
            )
        };
        let wait = match pace {
            Pace::Proceed(wait) if wait.is_zero() => return true,
            Pace::Proceed(wait) => Some(wait.min(MAX_REPORT_PACING_DELAY)),
            Pace::Reject => None,
        };
        throttled!(
            self.throttle,
            (Category::ReportRate, self.peer.ip()),
            warn,
            "[#{}] agent {} 的 report 超過每秒 {} 則（可連續 {} 則），{}",
            self.conn_id,
            agent.agent_id,
            REPORT_RATE_PER_SEC,
            REPORT_BURST,
            if wait.is_some() {
                "延後處理"
            } else {
                "不保存並回 storage_unavailable"
            }
        );
        match wait {
            Some(wait) => {
                tokio::time::sleep(wait).await;
                true
            }
            None => false,
        }
    }

    /// `inventory`（協商了 feature `inventory`；METRICS.md 第 6.2 節）：內容不符 `validate_inventory` →
    /// `invalid_report`（`KRS-W2108`）；距離上一則不到 60 秒 → `storage_unavailable`；保存後 `ack`，
    /// 寫入失敗 → `storage_unavailable`。計入訊息速率與連續無效訊息，不計入 report 的速率限制
    async fn handle_inventory(
        &mut self,
        ctx: &ConnContext,
        agent: &Agent,
        inv: InventoryReport,
        received_at: DateTime<Utc>,
    ) -> Result<Handled, Error> {
        self.bad_requests = 0;
        if !self.touch(ctx, agent) {
            return Ok(Handled::alive(self.reply_superseded(agent).await?));
        }
        let seq = inv.seq;
        if let Err(message) = validate_inventory(&inv.inventory) {
            throttled!(
                self.throttle,
                (Category::InvalidReport, self.peer.ip()),
                warn,
                "警告 {}：已拒絕 {}（agent {}）送出的資產資訊：內容不合法：{}。這一次的資產資訊（inventory seq={}）沒有保存，主控台仍顯示上一次的內容；指標回報不受影響。處理方式：確認這台主機的 Pulse 是正式版本，若是請以 pulse diag 產生診斷包並回報 [#{}]",
                CODE_INVALID_INVENTORY,
                agent.hostname.escape_debug(),
                agent.agent_id,
                message,
                seq,
                self.conn_id
            );
            ctx.registry
                .set_inventory_rejected(&agent.agent_id, self.conn_id, &message);
            let flow = self
                .reply(ServerMsg::error(
                    Some(seq),
                    ErrorCode::InvalidReport,
                    message,
                ))
                .await?;
            return Ok(Handled::alive(flow));
        }
        let now = Instant::now();
        let min_interval = Duration::from_millis(INVENTORY_MIN_INTERVAL_MS);
        if self
            .last_inventory
            .is_some_and(|t| now.saturating_duration_since(t) < min_interval)
        {
            debug!(
                "[#{}] agent {} 的 inventory seq={} 距離上一則不到 60 秒，不保存",
                self.conn_id, agent.agent_id, seq
            );
            let flow = self
                .reply(ServerMsg::error(
                    Some(seq),
                    ErrorCode::StorageUnavailable,
                    "inventory rate limit exceeded",
                ))
                .await?;
            return Ok(Handled::alive(flow));
        }
        self.last_inventory = Some(now);
        let Some(sink) = self.assets.clone() else {
            // 宣告 inventory 時一定有寫入佇列；防禦性處理
            return Ok(Handled::not_alive(self.reply_unknown_type().await?));
        };
        let received_ms = received_at.timestamp_millis();
        // ts 是收集資產的時間；無法解析或與收到的時間相差超過 5 分鐘（或早於 1 天）時以收到的時間代替
        let collected_ms = DateTime::parse_from_rfc3339(&inv.ts)
            .ok()
            .map(|t| t.timestamp_millis())
            .filter(|t| *t <= received_ms + 300_000 && *t >= received_ms - 86_400_000)
            .unwrap_or(received_ms);
        // 個資政策：介面位址與 MAC（privacy.inventory_network）在合併與摘要之前轉換
        let mut inventory = inv.inventory;
        self.privacy.inventory(&mut inventory);
        let write = crate::store::AssetWrite {
            agent_id: agent.agent_id.clone(),
            hostname: agent.hostname.clone(),
            peer: self.stored_peer.clone(),
            collected_ms,
            received_ms,
            inventory,
        };
        match sink.write(write, self.write_timeout).await {
            Ok(outcome) => {
                ctx.registry.set_assets(
                    &agent.agent_id,
                    self.conn_id,
                    AssetSummary {
                        logical_cores: outcome.logical_cores,
                        os_family: outcome.os_family.clone(),
                    },
                );
                debug!(
                    "[#{}] agent {} inventory seq={}（{}）",
                    self.conn_id,
                    agent.agent_id,
                    seq,
                    if outcome.first {
                        "首次收到資產"
                    } else if outcome.changed {
                        "資產已改變"
                    } else {
                        "與上次相同"
                    }
                );
                Ok(Handled::alive(self.reply(ServerMsg::Ack { seq }).await?))
            }
            Err(reason) => {
                debug!(
                    "[#{}] agent {} 的 inventory seq={} 未保存: {}",
                    self.conn_id, agent.agent_id, seq, reason
                );
                // 未保存：下一則不受 60 秒限制
                self.last_inventory = None;
                let flow = self
                    .reply(ServerMsg::error(
                        Some(seq),
                        ErrorCode::StorageUnavailable,
                        "report could not be stored",
                    ))
                    .await?;
                Ok(Handled::alive(flow))
            }
        }
    }

    async fn handle_registered(
        &mut self,
        ctx: &ConnContext,
        agent: &Agent,
        frame: Frame,
        received_at: DateTime<Utc>,
    ) -> Result<Handled, Error> {
        let msg = match classify(frame) {
            Incoming::TooLong => return Ok(Handled::not_alive(self.reply_too_long().await?)),
            Incoming::BadRequest { message, seq } => {
                return Ok(Handled::not_alive(
                    self.reply_bad_request(seq, message).await?,
                ));
            }
            // 協商了 renew 的 renew_request 不合法：bad_request；其他（沒有協商、或一般連線上的
            // deregister）與舊版相同，回 unknown_type
            Incoming::InvalidLifecycle { kind, message } => {
                let flow = if kind == "renew_request" && agent.renew && self.enroll.is_some() {
                    self.reply_bad_request(None, format!("invalid renew_request: {message}"))
                        .await?
                } else if kind == "inventory" && agent.session.inventory {
                    // 協商了 inventory：格式錯誤（型別、ts）與其他訊息相同，bad_request（seq 為 null）
                    self.reply_bad_request(None, format!("invalid inventory: {message}"))
                        .await?
                } else {
                    self.reply_unknown_type().await?
                };
                return Ok(Handled::not_alive(flow));
            }
            Incoming::Msg(msg) => msg,
        };

        // 只有合法訊息（hello、heartbeat、report、協商了 renew 的 renew_request）會把連續無效訊息的
        // 計數歸零
        match msg {
            ClientMsg::RenewRequest(req) if agent.renew => match self.enroll.clone() {
                Some(svc) => self.handle_renew(&svc, agent, *req).await,
                None => Ok(Handled::not_alive(self.reply_unknown_type().await?)),
            },
            ClientMsg::Inventory(inv) if agent.session.inventory => {
                self.handle_inventory(ctx, agent, *inv, received_at).await
            }
            // 未知 type 不是合法訊息：不重置計數，也不延後閒置期限。沒有協商 renew 的 renew_request、
            // 一般連線上的 deregister、沒有協商 inventory 的 inventory 與舊版相同，視為不認得的 type
            ClientMsg::Unknown
            | ClientMsg::RenewRequest(_)
            | ClientMsg::Deregister(_)
            | ClientMsg::Inventory(_) => Ok(Handled::not_alive(self.reply_unknown_type().await?)),
            ClientMsg::Hello { seq, .. } => {
                self.bad_requests = 0;
                if !self.touch(ctx, agent) {
                    return Ok(Handled::alive(self.reply_superseded(agent).await?));
                }
                debug!(
                    "[#{}] agent {} 重複送出 hello",
                    self.conn_id, agent.agent_id
                );
                let flow = self
                    .reply(ServerMsg::error(
                        Some(seq),
                        ErrorCode::AlreadyRegistered,
                        "hello already received on this connection",
                    ))
                    .await?;
                Ok(Handled::alive(flow))
            }
            ClientMsg::Heartbeat { seq, .. } => {
                self.bad_requests = 0;
                if !self.touch(ctx, agent) {
                    return Ok(Handled::alive(self.reply_superseded(agent).await?));
                }
                trace!(
                    "[#{}] agent {} heartbeat seq={}",
                    self.conn_id, agent.agent_id, seq
                );
                Ok(Handled::alive(self.reply(ServerMsg::Ack { seq }).await?))
            }
            ClientMsg::Report { seq, ts, body } => {
                // 依協商的格式取出內容；格式不符（欄位缺漏、型別錯誤）與 v1 的解析失敗相同：
                // bad_request（seq 為 null）、不算合法訊息
                let content = if agent.session.metrics {
                    body.metric_list()
                        .map(|metrics| ReportContent::Metrics(metrics.to_vec()))
                } else {
                    body.legacy_fields().map(|l| ReportContent::Legacy {
                        // 線上型別為 f64；驗證、保存與日誌沿用 f32（A14，見 server_cpu_usage）
                        cpu_usage: server_cpu_usage(l.cpu_usage),
                        total_memory: l.total_memory,
                        used_memory: l.used_memory,
                    })
                };
                let content = match content {
                    Ok(content) => content,
                    Err(message) => {
                        return Ok(Handled::not_alive(
                            self.reply_bad_request(None, message).await?,
                        ));
                    }
                };
                // 行程排行（METRICS.md 第 5 節）：只在協商了 processes 的連線上解析；型別錯誤為
                // bad_request，內容不符限制在下方與指標一起回 invalid_report。沒有協商時一律忽略
                let processes = if agent.session.processes {
                    match body.process_list() {
                        Ok(p) => p.cloned(),
                        Err(message) => {
                            return Ok(Handled::not_alive(
                                self.reply_bad_request(None, message).await?,
                            ));
                        }
                    }
                } else {
                    None
                };
                // durable（A5）：必須帶 boot_id／report_seq；缺漏、型別錯誤或不符限制與其他欄位
                // 相同，回 bad_request（seq 為 null）、不算合法訊息。未協商 durable 時一律忽略
                let dedup = if agent.session.durable {
                    match body.durable_seq() {
                        Ok(durable) => Some(DedupKey {
                            boot_id: durable.boot_id.to_ascii_lowercase(),
                            report_seq: durable.report_seq,
                        }),
                        Err(message) => {
                            return Ok(Handled::not_alive(
                                self.reply_bad_request(None, message).await?,
                            ));
                        }
                    }
                } else {
                    None
                };
                self.bad_requests = 0;
                if !self.pace_report(agent).await {
                    if !self.touch(ctx, agent) {
                        return Ok(Handled::alive(self.reply_superseded(agent).await?));
                    }
                    ctx.reports.stats().record(WriteOutcome::RateLimited);
                    let flow = self
                        .reply(ServerMsg::error(
                            Some(seq),
                            ErrorCode::StorageUnavailable,
                            "report rate limit exceeded",
                        ))
                        .await?;
                    return Ok(Handled::alive(flow));
                }
                let checked = content.validate().and_then(|()| match &processes {
                    Some(p) => validate_process_list(p).map_err(|e| format!("processes: {e}")),
                    None => Ok(()),
                });
                if let Err(message) = checked {
                    // 不寫檔，但仍視為存活
                    if !self.touch(ctx, agent) {
                        return Ok(Handled::alive(self.reply_superseded(agent).await?));
                    }
                    throttled!(
                        self.throttle,
                        (Category::InvalidReport, self.peer.ip()),
                        warn,
                        "[#{}] agent {} 的 report seq={} 無效: {}",
                        self.conn_id,
                        agent.agent_id,
                        seq,
                        message
                    );
                    let flow = self
                        .reply(ServerMsg::error(
                            Some(seq),
                            ErrorCode::InvalidReport,
                            message,
                        ))
                        .await?;
                    return Ok(Handled::alive(flow));
                }

                let mut summary = content.summary();
                if let Some(key) = &dedup {
                    summary.push_str(&format!(
                        " boot_id={} report_seq={} ts={}",
                        key.boot_id,
                        key.report_seq,
                        ts.escape_debug()
                    ));
                }
                let mut processes = processes;
                if let Some(list) = processes.as_mut() {
                    self.privacy.processes(list);
                }
                let record = ReportRecord::new(
                    received_at,
                    self.stored_peer.clone(),
                    agent.agent_id.clone(),
                    agent.hostname.clone(),
                    seq,
                    ts,
                    content.into_body(),
                )
                .with_dedup(dedup)
                .with_processes(processes);
                // ack 語意 = 已寫入資料庫或資料檔；等待 writer 回覆（受寫出逾時限制）
                let revived = AtomicBool::new(false);
                let outcome = store_report(
                    &ctx.registry,
                    &ctx.reports,
                    self.conn_id,
                    record,
                    self.write_timeout,
                    &revived,
                )
                .await;
                // 等待期間任何一次更新讓 agent 從離線恢復，都記錄「恢復上線」（R2-K13）
                if revived.load(Ordering::Relaxed) {
                    self.on_touch(agent, Touch::Revived);
                }
                match outcome {
                    StoreOutcome::Stale => {
                        debug!(
                            "[#{}] agent {} 的連線已被取代，report seq={} 不寫檔",
                            self.conn_id, agent.agent_id, seq
                        );
                        Ok(Handled::alive(self.reply_superseded(agent).await?))
                    }
                    StoreOutcome::Stored(_) => {
                        debug!(
                            "[#{}] agent {} report seq={} {}",
                            self.conn_id, agent.agent_id, seq, summary
                        );
                        Ok(Handled::alive(self.reply(ServerMsg::Ack { seq }).await?))
                    }
                    StoreOutcome::Unavailable(reason) => {
                        // writer 已記錄 error；每個連線、每筆 report 都可能遇到，這裡只記 debug 避免洗版
                        debug!(
                            "[#{}] agent {} 的 report seq={} 未保存: {}",
                            self.conn_id, agent.agent_id, seq, reason
                        );
                        let flow = self
                            .reply(ServerMsg::error(
                                Some(seq),
                                ErrorCode::StorageUnavailable,
                                "report could not be stored",
                            ))
                            .await?;
                        Ok(Handled::alive(flow))
                    }
                    StoreOutcome::Unknown(reason) => {
                        // 既不能說「已保存」也不能說「未保存」：不回應，直接關閉連線
                        throttled!(
                            self.throttle,
                            Category::WriteUnknown,
                            warn,
                            "[#{}] agent {} 的 report seq={} 寫入結果未知（{}），不回應並關閉連線",
                            self.conn_id,
                            agent.agent_id,
                            seq,
                            reason
                        );
                        ctx.registry.set_close_reason(
                            &agent.agent_id,
                            self.conn_id,
                            crate::registry::OfflineReason::WriteUnknown,
                        );
                        self.close().await?;
                        Ok(Handled::alive(Flow::Close))
                    }
                }
            }
        }
    }
}

/// 對方未送 close_notify 就斷線（UnexpectedEof）、連線被重設（10054）或中止、寫入時管線已斷，
/// 這些都是 agent 端關機、斷網時的常見情況，不視為伺服器錯誤
pub fn is_peer_disconnect(e: &Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
    )
}

// 為非同步 I/O 加上逾時，逾時時轉換為 std::io::Error
pub async fn with_timeout<T>(
    duration: Duration,
    action: &str,
    fut: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    timeout(duration, fut)
        .await
        .map_err(|_| Error::new(ErrorKind::TimedOut, format!("{}逾時", action)))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{WriteRequest, report_channel};

    const AGENT: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";

    fn hello_info() -> HelloInfo {
        HelloInfo {
            agent_id: AGENT.into(),
            hostname: "web-01".into(),
            agent_version: "0.1.0".into(),
            proto: 1,
            host: None,
            peer: "127.0.0.1:1".into(),
            offline_after: Duration::from_secs(5),
            client_chain: None,
        }
    }

    fn record(seq: u64) -> ReportRecord {
        ReportRecord::new(
            Utc::now(),
            "p".into(),
            AGENT.into(),
            "web-01".into(),
            seq,
            "t".into(),
            RecordBody::Legacy {
                cpu_usage: 1.0,
                total_memory: 2,
                used_memory: 1,
            },
        )
    }

    #[test]
    fn report_detail_collects_f4_metrics_and_estimates_missing_usage() {
        let metrics: Vec<Metric> = serde_json::from_str(
            r#"[{"name":"system.uptime","value":1532941,"unit":"s"},
                {"name":"system.cpu.load_5m","value":1.98,"unit":"1"},
                {"name":"system.swap.total","value":4294967296,"unit":"By"},
                {"name":"system.swap.used","value":0,"unit":"By"},
                {"name":"system.process.count","value":312,"unit":"{processes}"},
                {"name":"system.filesystem.total","value":1000,"unit":"By","labels":{"mountpoint":"/","fstype":"ext4"}},
                {"name":"system.filesystem.available","value":250,"unit":"By","labels":{"mountpoint":"/","fstype":"ext4"}},
                {"name":"system.filesystem.total","value":100,"unit":"By","labels":{"mountpoint":"/data"}},
                {"name":"system.filesystem.usage","value":70.0,"unit":"%","labels":{"mountpoint":"/data"}},
                {"name":"system.network.received","value":98765,"unit":"By","labels":{"device":"eth0"}},
                {"name":"system.disk.written","value":5,"unit":"By","labels":{"device":"dm-0"}},
                {"name":"system.uptime","value":7,"unit":"min"}]"#,
        )
        .unwrap();
        let d = report_detail(&metrics);
        assert_eq!(d.uptime_secs, Some(1_532_941), "單位不符的同名指標不採用");
        assert_eq!(d.load_5m, Some(1.98));
        assert_eq!(d.swap_percent(), Some(0.0));
        assert_eq!(d.process_count, Some(312));
        let root = &d.filesystems[0];
        assert_eq!(root.mountpoint, "/");
        assert_eq!(root.fstype.as_deref(), Some("ext4"));
        assert_eq!(
            (root.usage, root.estimated, root.used),
            (Some(75.0), true, Some(750))
        );
        let data = &d.filesystems[1];
        assert_eq!((data.usage, data.estimated), (Some(70.0), false));
        assert_eq!(d.top_filesystem().unwrap().mountpoint, "/");
        assert_eq!(d.network[0].rx, Some(98_765));
        assert_eq!(d.disks[0].device, "dm-0");
        assert_eq!(d.disks[0].tx, Some(5));
    }

    #[test]
    fn last_report_from_metrics_uses_the_standard_metrics() {
        let metrics: Vec<Metric> = serde_json::from_str(
            r#"[{"name":"system.cpu.usage","value":17.84,"unit":"%"},
                {"name":"system.memory.total","value":51539607552,"unit":"By"},
                {"name":"system.memory.used","value":38833356800,"unit":"By"},
                {"name":"system.memory.used","value":1,"unit":"By","labels":{"kind":"x"}}]"#,
        )
        .unwrap();
        let r = ReportRecord::new(
            Utc::now(),
            "p".into(),
            AGENT.into(),
            "h".into(),
            3,
            "t".into(),
            RecordBody::Metrics { metrics },
        );
        let last = last_report_of(&r);
        assert_eq!(last.cpu_usage, Some(17.84));
        assert_eq!(last.total_memory, Some(49152));
        assert_eq!(last.used_memory, Some(37034));
        assert_eq!(last.metrics, Some(4));
        // 沒有標準指標時不輸出這些欄位
        let r = ReportRecord::new(
            Utc::now(),
            "p".into(),
            AGENT.into(),
            "h".into(),
            4,
            "t".into(),
            RecordBody::Metrics {
                metrics: vec![Metric::new("app.queue.depth", 3, "{items}")],
            },
        );
        let v = serde_json::to_value(last_report_of(&r)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"recv_at": v["recv_at"], "seq": 4, "ts": "t", "metrics": 1})
        );
        // v1：與加入 v2 前的輸出相同
        let v = serde_json::to_value(last_report_of(&record(5))).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"recv_at": v["recv_at"], "seq": 5, "ts": "t",
                "cpu_usage": 1.0, "total_memory": 2, "used_memory": 1})
        );
    }

    fn last_report_seq(registry: &Registry) -> Option<u64> {
        registry.snapshot(Utc::now()).agents[0]
            .last_report
            .as_ref()
            .map(|r| r.seq)
    }

    /// 模擬 writer：取出一筆要求、認領後以 `result` 回覆
    fn fake_writer(
        mut rx: tokio::sync::mpsc::Receiver<WriteRequest>,
        result: Result<(), String>,
    ) -> tokio::task::JoinHandle<Option<u64>> {
        tokio::spawn(async move {
            let req = rx.recv().await?;
            assert!(req.ticket.claim());
            let seq = req.record.seq;
            let status = match result {
                Ok(()) => WriteStatus::Written,
                Err(reason) => WriteStatus::Failed(reason),
            };
            let _ = req.reply.send(status);
            Some(seq)
        })
    }

    #[tokio::test]
    async fn store_report_success_updates_last_report() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, rx) = report_channel(4);
        let writer = fake_writer(rx, Ok(()));

        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(7),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Updated));
        assert_eq!(writer.await.unwrap(), Some(7));
        assert_eq!(last_report_seq(&registry), Some(7));
    }

    #[tokio::test]
    async fn duplicate_report_is_acked_without_updating_last_report() {
        // 去重（durable，A5）：去重鍵隨紀錄交給 writer；writer 回 Duplicate（已保存過）時照常 ack，
        // 但 last_report 不改成重送的舊資料
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let writer = tokio::spawn(async move {
            let mut keys = Vec::new();
            for status in [WriteStatus::Written, WriteStatus::Duplicate] {
                let req = rx.recv().await.unwrap();
                assert!(req.ticket.claim());
                keys.push(req.record.dedup.clone());
                let _ = req.reply.send(status);
            }
            keys
        });
        let key = |report_seq| DedupKey {
            boot_id: "0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e".into(),
            report_seq,
        };
        for (seq, report_seq) in [(9, 4), (8, 3)] {
            let outcome = store_report(
                &registry,
                &sink,
                1,
                record(seq).with_dedup(Some(key(report_seq))),
                Duration::from_secs(1),
                &AtomicBool::new(false),
            )
            .await;
            assert_eq!(outcome, StoreOutcome::Stored(Touch::Updated));
        }
        assert_eq!(writer.await.unwrap(), vec![Some(key(4)), Some(key(3))]);
        assert_eq!(last_report_seq(&registry), Some(9));
    }

    #[tokio::test]
    async fn store_report_write_failure_is_unavailable() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, rx) = report_channel(4);
        let writer = fake_writer(rx, Err("disk full".into()));

        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(8),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Unavailable("disk full".into()));
        assert_eq!(writer.await.unwrap(), Some(8));
        // 未保存的資料不得出現在 last_report
        assert_eq!(last_report_seq(&registry), None);
    }

    #[tokio::test]
    async fn store_report_timeout_is_unavailable_and_cancels_the_write() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        // writer 一直不處理（尚未認領）
        let (sink, mut rx) = report_channel(4);
        let started = std::time::Instant::now();
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(9),
            Duration::from_millis(50),
            &AtomicBool::new(false),
        )
        .await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(last_report_seq(&registry), None);
        // K-C1：已回 storage_unavailable 的要求，writer 之後不得寫入（認領失敗）
        let req = rx.recv().await.unwrap();
        assert!(!req.ticket.claim());
    }

    #[tokio::test]
    async fn store_report_claimed_but_unfinished_is_unknown() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        // writer 認領後卡住（例如磁碟沒有回應）
        let (sink, mut rx) = report_channel(4);
        let holder = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(req);
        });
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(9),
            Duration::from_millis(50),
            &AtomicBool::new(false),
        )
        .await;
        // 不能回 storage_unavailable（資料可能已寫入），也不能回 ack
        assert!(matches!(outcome, StoreOutcome::Unknown(_)), "{outcome:?}");
        assert_eq!(last_report_seq(&registry), None);
        holder.abort();
    }

    // R2-T2：以暫停的 tokio 時鐘執行，邊界判定不受實際排程延遲影響
    #[tokio::test(start_paused = true)]
    async fn store_report_claimed_write_finishing_within_grace_is_stored() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            // 超過逾時（500 ms）才完成，但在寬限（再 500 ms）內
            tokio::time::sleep(Duration::from_millis(800)).await;
            let _ = req.reply.send(WriteStatus::Written);
        });
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(11),
            Duration::from_millis(500),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Updated));
        assert_eq!(last_report_seq(&registry), Some(11));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn store_report_touches_again_after_slow_write() {
        // K-C4：等待寫檔期間被 sweep 判定離線，寫入完成後應恢復上線
        let registry = Arc::new(Registry::new());
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let r = registry.clone();
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            let swept = r.sweep(std::time::Instant::now() + Duration::from_secs(60));
            assert_eq!(swept.went_offline.len(), 1);
            let _ = req.reply.send(WriteStatus::Written);
        });
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(12),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Revived));
        assert_eq!(registry.counts(), (1, 1));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn slow_disk_longer_than_offline_threshold_does_not_mark_agent_offline() {
        // K-C4：磁碟卡住超過離線門檻（此處 3 秒）時，等待期間每秒 touch 本連線，不會被 sweep 標記離線。
        // R2-T2：touch 間隔 1 秒與門檻 3 秒之間留 2 秒餘裕，卡住時間比門檻多 1.5 秒
        // （registry 以 std 的時鐘判定離線，無法使用暫停的 tokio 時鐘）
        let registry = Arc::new(Registry::new());
        let mut info = hello_info();
        info.offline_after = Duration::from_secs(3);
        let _reg = registry
            .register(1, info, std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            tokio::time::sleep(Duration::from_millis(4500)).await;
            let _ = req.reply.send(WriteStatus::Written);
        });
        // 模擬 monitor：每 100 ms sweep 一次
        let r = registry.clone();
        let sweeper = tokio::spawn(async move {
            let mut went_offline = 0;
            loop {
                went_offline += r.sweep(std::time::Instant::now()).went_offline.len();
                if went_offline > 0 {
                    return went_offline;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(13),
            Duration::from_secs(6),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Updated));
        assert!(!sweeper.is_finished(), "等待寫檔期間不應被標記離線");
        sweeper.abort();
        assert_eq!(registry.counts(), (1, 1));
        writer.await.unwrap();

        // 已被取代的舊連線等待期間不會 touch 到新連線的狀態
        let _new = registry
            .register(2, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        assert_eq!(
            registry.touch(AGENT, 1, std::time::Instant::now(), Utc::now()),
            Touch::Stale
        );
    }

    #[tokio::test]
    async fn store_report_queue_full_times_out() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        // 容量 1 且已塞滿、沒人讀取 → 連放入佇列都會逾時
        let (sink, _rx) = report_channel(1);
        let s2 = sink.clone();
        let _stuck = tokio::spawn(async move { s2.write(record(1)).await });
        while sink.pending() < 1 {
            tokio::task::yield_now().await;
        }
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(2),
            Duration::from_millis(50),
            &AtomicBool::new(false),
        )
        .await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
        assert_eq!(last_report_seq(&registry), None);
        // 逾時的那筆沒有進入佇列
        assert_eq!(sink.pending(), 1);
    }

    #[tokio::test]
    async fn store_report_writer_stopped_is_unavailable() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, rx) = report_channel(4);
        drop(rx);
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(3),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn store_report_from_superseded_connection_writes_nothing() {
        let registry = Registry::new();
        let _old = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let _new = registry
            .register(2, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);

        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(9),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stale);
        assert!(rx.try_recv().is_err(), "被取代的連線不得寫檔");
        assert_eq!(last_report_seq(&registry), None);
    }

    #[tokio::test(start_paused = true)]
    async fn revival_during_keepalive_is_reported() {
        // R2-K13／R2-T2：等待寫檔期間被 sweep 判定離線，由「等待中的定期 touch」恢復上線
        // （寫入完成後的 touch 只看到 Updated），仍要回報恢復上線
        let registry = Arc::new(Registry::new());
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let r = registry.clone();
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            assert_eq!(
                r.sweep(std::time::Instant::now() + Duration::from_secs(60))
                    .went_offline
                    .len(),
                1
            );
            // 跨過至少一次定期 touch（每 1 秒）
            tokio::time::sleep(STORE_KEEPALIVE_INTERVAL * 2).await;
            assert_eq!(r.counts(), (1, 1), "定期 touch 已恢復上線");
            let _ = req.reply.send(WriteStatus::Written);
        });
        let revived = AtomicBool::new(false);
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(14),
            Duration::from_secs(5),
            &revived,
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Revived));
        assert!(revived.load(Ordering::Relaxed));
        writer.await.unwrap();

        // 寫入失敗時也回報（恢復上線與是否保存無關）
        let (sink, mut rx) = report_channel(4);
        let r = registry.clone();
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            r.sweep(std::time::Instant::now() + Duration::from_secs(60));
            tokio::time::sleep(STORE_KEEPALIVE_INTERVAL * 2).await;
            let _ = req.reply.send(WriteStatus::Failed("disk full".into()));
        });
        let revived = AtomicBool::new(false);
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(15),
            Duration::from_secs(5),
            &revived,
        )
        .await;
        assert!(matches!(outcome, StoreOutcome::Unavailable(_)));
        assert!(revived.load(Ordering::Relaxed));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_guard_marks_offline_even_if_the_task_panics() {
        // R2-K20：連線工作 panic 時仍以 conn_id 標記離線
        let registry = Arc::new(Registry::new());
        let r = registry.clone();
        let task = tokio::spawn(async move {
            let _reg = r
                .register(7, hello_info(), std::time::Instant::now(), Utc::now())
                .unwrap();
            let _guard = DisconnectGuard::new(r.clone(), AGENT.to_string(), 7);
            panic!("連線工作異常");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert_eq!(registry.counts(), (0, 1));

        // 正常結束：disconnect 回報狀態變化，之後 drop 不重複處理
        let _reg = registry
            .register(8, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let guard = DisconnectGuard::new(registry.clone(), AGENT.to_string(), 8);
        assert!(guard.disconnect());
        // 已被新連線取代的舊連線：不影響新連線
        let _new = registry
            .register(9, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        drop(DisconnectGuard::new(registry.clone(), AGENT.to_string(), 8));
        assert_eq!(registry.counts(), (1, 1));
    }

    #[test]
    fn peer_disconnect_kinds() {
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
        ] {
            assert!(is_peer_disconnect(&Error::new(kind, "x")));
        }
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::InvalidData,
            ErrorKind::Other,
        ] {
            assert!(!is_peer_disconnect(&Error::new(kind, "x")));
        }
    }

    #[test]
    fn deadline_after_does_not_panic_on_overflow() {
        let before = Instant::now();
        assert!(deadline_after(Duration::from_secs(5)) >= before + Duration::from_secs(5));
        // 極大值不得 panic
        assert!(deadline_after(Duration::MAX) >= before);
    }

    #[test]
    fn classify_rejects_long_ts() {
        let line = format!(
            r#"{{"type":"heartbeat","seq":1,"ts":"{}"}}"#,
            "x".repeat(65)
        );
        assert!(matches!(
            classify(Frame::Line(line)),
            Incoming::BadRequest { message: m, seq: None } if m.contains("ts")
        ));
        let line = format!(
            r#"{{"type":"heartbeat","seq":1,"ts":"{}"}}"#,
            "x".repeat(64)
        );
        assert!(matches!(classify(Frame::Line(line)), Incoming::Msg(_)));
        // R2-K8：ts 含行分隔字元 → bad_request
        let line = r#"{"type":"heartbeat","seq":1,"ts":"2026 x"}"#.to_string();
        assert!(matches!(
            classify(Frame::Line(line)),
            Incoming::BadRequest { message: m, seq: None } if m.contains("ts")
        ));
    }

    #[test]
    fn lifecycle_features_require_an_authenticated_v2_connection() {
        let both = vec!["metrics".to_string(), "renew".into(), "deregister".into()];
        assert_eq!(lifecycle_features(true, true, true, &both), (true, true));
        // 未以用戶端憑證證明身分（未啟用 mTLS）：不使用，hello_ack 不列出
        assert_eq!(lifecycle_features(false, true, true, &both), (false, false));
        // v1、沒有註冊服務（JSONL）、client 沒有宣告
        assert_eq!(lifecycle_features(true, false, true, &both), (false, false));
        assert_eq!(lifecycle_features(true, true, false, &both), (false, false));
        assert_eq!(
            lifecycle_features(true, true, true, &["renew".to_string()]),
            (true, false)
        );
        assert_eq!(lifecycle_features(true, true, true, &[]), (false, false));
    }

    #[test]
    fn classify_keeps_the_seq_of_an_invalid_hello() {
        // F5：hello 的 ts 不合規定 → bad_request 帶回 hello 的 seq
        let hello = r#"{"type":"hello","seq":7,"proto":1,"agent_id":"6f1c0e4a-1b2c-4d3e-9f00-112233445566","hostname":"h","agent_version":"1","heartbeat_interval_ms":1000,"ts":"TS"}"#;
        let line = hello.replace("TS", &"x".repeat(65));
        assert!(matches!(
            classify(Frame::Line(line)),
            Incoming::BadRequest { message: m, seq: Some(7) } if m.contains("ts")
        ));
        // 欄位缺漏（無法解析）仍能取回 seq
        let line = hello.replace(r#""hostname":"h","#, "");
        assert!(matches!(
            classify(Frame::Line(line)),
            Incoming::BadRequest { seq: Some(7), .. }
        ));
        // 壞 JSON、不是 hello：不帶 seq
        assert!(matches!(
            classify(Frame::Line("{".to_string())),
            Incoming::BadRequest { seq: None, .. }
        ));
        assert!(matches!(
            classify(Frame::Line(r#"{"type":"report","seq":3}"#.to_string())),
            Incoming::BadRequest { seq: None, .. }
        ));
        assert!(matches!(
            classify(Frame::InvalidUtf8),
            Incoming::BadRequest { seq: None, .. }
        ));
    }
}
