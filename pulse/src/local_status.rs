//! 執行中的 Pulse 記錄自己的狀態，並定期寫出本機狀態檔 `status.json`（格式見 [`crate::status_file`]）。
//!
//! 各任務（連線、收集、本機緩衝）透過 [`record`] 更新狀態；全域的追蹤器只在常駐模式初始化（[`init`]），
//! 單元測試與其他子命令中 [`record`] 什麼都不做。寫出由 [`run_writer`] 負責：每
//! [`status_file::WRITE_INTERVAL`]（10 秒）一次，狀態變化（連線狀態、錯誤代碼、緩衝狀態、丟棄資料）
//! 時另外寫，但兩次寫入至少間隔 [`status_file::MIN_WRITE_GAP`]。補送中待送筆數每秒都在變，
//! 不算狀態變化，跟著定期寫入更新即可。
//!
//! 磁碟負擔：一份約 3～5 KB、每 10 秒一次（約 0.4 KB/s，一天約 40 MB 的寫入量），不 fsync，
//! 多數寫入在作業系統的快取中合併；比本機緩衝每 5 秒一筆取樣（不 fsync，每 10 秒 fsync 一次）小。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::codes;
use crate::spool::SharedSpool;
use crate::status_file::{self as sf, CodeNote, StatusFile, ts};

/// 連續寫入失敗這麼久（狀態檔開始被判為過時，[`sf::STALE_AFTER`]）時記 `ERROR`（`log-ux.md` §4.5）
const WRITE_FAILURE_ERROR_AFTER: Duration = sf::STALE_AFTER;
/// 之後的 `ERROR` 摘要間隔（`log-ux.md` §5.1「需人工處理」）
const WRITE_FAILURE_SUMMARY_EVERY: Duration = Duration::from_secs(600);

/// `status.write_failed`（5020）的一次處置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteFailureLog {
    /// 第一次失敗
    Warn,
    /// 連續失敗滿 [`WRITE_FAILURE_ERROR_AFTER`]，或距上一則 `ERROR` 滿 [`WRITE_FAILURE_SUMMARY_EVERY`]：
    /// 寫 stderr；`suppressed` 為上一則之後只記 `DEBUG` 的次數，`failing` 為已連續失敗多久
    Error { suppressed: u64, failing: Duration },
    /// 其間的失敗
    Debug,
}

/// 狀態檔寫入失敗與恢復的記錄規則（`log-ux.md` §4.5：`status.write_failed` 5020、
/// `status.write_restored` 5021）
#[derive(Debug, Default)]
struct WriteFailures {
    /// 這一段連續失敗的開始
    since: Option<tokio::time::Instant>,
    /// 最近一則 `ERROR`
    last_error: Option<tokio::time::Instant>,
    suppressed: u64,
}

impl WriteFailures {
    fn failed(&mut self, now: tokio::time::Instant) -> WriteFailureLog {
        let Some(since) = self.since else {
            self.since = Some(now);
            return WriteFailureLog::Warn;
        };
        let failing = now.duration_since(since);
        let due = match self.last_error {
            None => failing >= WRITE_FAILURE_ERROR_AFTER,
            Some(last) => now.duration_since(last) >= WRITE_FAILURE_SUMMARY_EVERY,
        };
        if due {
            self.last_error = Some(now);
            WriteFailureLog::Error {
                suppressed: std::mem::take(&mut self.suppressed),
                failing,
            }
        } else {
            self.suppressed += 1;
            WriteFailureLog::Debug
        }
    }

    /// 寫入成功：之前在失敗中時回傳連續失敗了多久（記 `status.write_restored`）
    fn succeeded(&mut self, now: tokio::time::Instant) -> Option<Duration> {
        let since = self.since.take()?;
        self.last_error = None;
        self.suppressed = 0;
        Some(now.duration_since(since))
    }
}

/// 最近 1 小時計數的時間格（每分鐘一格）
const WINDOW_SLOTS: u64 = 60;

/// 每分鐘一格的計數
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    minute: u64,
    sampled: u64,
    acked: u64,
    dropped_disconnected: u64,
    rejected_by_server: u64,
}

/// 追蹤器內部的狀態：`file` 是下一次寫出的內容（緩衝的統計在寫出時才向緩衝取得）
#[derive(Debug)]
pub struct State {
    file: StatusFile,
    started: Instant,
    slots: VecDeque<Slot>,
    /// 目前這次中斷開始的時間（單調時鐘）與當時的牆鐘
    outage_started: Option<(Instant, DateTime<Utc>)>,
    /// 這次中斷已記錄過 `conn.outage`
    outage_logged: bool,
    /// 目前這條連線是否協商了 durable
    connected_durable: Option<bool>,
    /// 上一條連線是否協商了 durable（中斷中沿用）
    last_durable: Option<bool>,
    buffer_disabled: bool,
    buffer_error: Option<String>,
    retry: Option<(DateTime<Utc>, &'static str)>,
    changed: bool,
    /// 不寫入狀態檔的 agent_id（未啟用 mTLS 時）
    secret_id: Option<String>,
}

/// 註冊成功時的資訊
#[derive(Debug, Clone)]
pub struct Registered {
    pub tls_version: Option<String>,
    pub server_verified: bool,
    pub proto: u32,
    pub features: Vec<String>,
    pub durable: bool,
    /// hello_ack 的 `server_time`（v2）
    pub server_time: Option<DateTime<Utc>>,
}

/// 斷線時的資訊
#[derive(Debug, Clone)]
pub struct Disconnected {
    /// 對應的錯誤代碼（無法歸類時為 `None`）
    pub code: Option<&'static str>,
    /// 斷線原因（已清理的單行文字）
    pub message: String,
    /// 是否需要人工處理（會一直重試但不會成功）
    pub attention: bool,
    pub consecutive_failures: u64,
    pub retry_in: Duration,
}

/// 中斷後恢復時的摘要（`conn.restored`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub outage: Duration,
    pub attempts: u64,
}

impl State {
    fn new(base: StatusFile) -> Self {
        Self {
            file: base,
            started: Instant::now(),
            slots: VecDeque::new(),
            outage_started: None,
            outage_logged: false,
            connected_durable: None,
            last_durable: None,
            buffer_disabled: false,
            buffer_error: None,
            retry: None,
            changed: true,
            secret_id: None,
        }
    }

    fn slot(&mut self) -> &mut Slot {
        let minute = self.started.elapsed().as_secs() / 60;
        if self.slots.back().is_none_or(|s| s.minute != minute) {
            self.slots.push_back(Slot {
                minute,
                ..Slot::default()
            });
        }
        while self
            .slots
            .front()
            .is_some_and(|s| s.minute + WINDOW_SLOTS <= minute)
        {
            self.slots.pop_front();
        }
        self.slots.back_mut().expect("剛加入")
    }

    fn window(&mut self) -> sf::Window1h {
        // 先以目前的分鐘整理（丟掉 1 小時以前的格子）
        let _ = self.slot();
        let mut w = sf::Window1h::default();
        for s in &self.slots {
            w.sampled += s.sampled;
            w.acked += s.acked;
            w.dropped_disconnected += s.dropped_disconnected;
            w.rejected_by_server += s.rejected_by_server;
        }
        w
    }

    /// 取得一筆取樣
    pub fn sampled(&mut self) {
        crate::metrics::global().sampled.inc();
        self.slot().sampled += 1;
    }

    /// 不補送時（at-most-once）因未連線、寫出中或上一筆未回應而丟掉的取樣
    pub fn dropped_disconnected(&mut self) {
        crate::metrics::global().dropped_disconnected.inc();
        self.slot().dropped_disconnected += 1;
    }

    /// 送出了一筆 report
    pub fn report_sent(&mut self) {
        crate::metrics::global().report_sent();
        self.file.reporting.last_report_at = Some(ts(Utc::now()));
    }

    /// Kairos 確認了一筆 report
    pub fn report_acked(&mut self) {
        crate::metrics::global().report_answered(true);
        self.slot().acked += 1;
        self.file.reporting.last_ack_at = Some(ts(Utc::now()));
        if self.retry.take().is_some() {
            self.changed = true;
        }
    }

    /// Kairos 拒收或無法保存一筆 report
    pub fn report_rejected(&mut self) {
        crate::metrics::global().report_answered(false);
        self.slot().rejected_by_server += 1;
    }

    /// durable：Kairos 暫時無法保存，退避到 `at` 後重送（`code` 為 `PLS-W4101`／`PLS-W2203`）
    pub fn server_backoff(&mut self, at: DateTime<Utc>, code: &'static str) {
        if self.retry.is_none_or(|(_, c)| c != code) {
            self.changed = true;
        }
        self.retry = Some((at, code));
    }

    /// 開始連線（啟動時）
    pub fn connecting(&mut self) {
        self.file.connection.state = "connecting".into();
        self.changed = true;
    }

    /// 註冊成功；之前有中斷時回傳中斷的摘要
    pub fn registered(&mut self, info: Registered) -> Option<Restored> {
        let m = crate::metrics::global();
        m.registrations.inc();
        m.connection_reset();
        let now = Utc::now();
        let c = &mut self.file.connection;
        let outage = self.outage_started.take();
        let restored = outage.map(|(at, _)| Restored {
            outage: at.elapsed(),
            attempts: c.consecutive_failures,
        });
        if let Some((_, from)) = outage {
            c.last_outage_from = Some(ts(from));
            c.last_outage_until = Some(ts(now));
        }
        c.state = "registered".into();
        c.tls_version = info.tls_version;
        c.server_verified = info.server_verified;
        c.proto = Some(info.proto);
        c.features = info.features;
        c.since = Some(ts(now));
        c.consecutive_failures = 0;
        c.next_retry_at = None;
        c.last_error_code = None;
        c.last_error = None;
        c.last_registered_at = Some(ts(now));
        c.last_rejection = None;
        self.outage_logged = false;
        self.connected_durable = Some(info.durable);
        self.last_durable = Some(info.durable);
        if let Some(server) = info.server_time {
            let clock = &mut self.file.clock;
            clock.offset_ms = Some((now - server).num_milliseconds());
            clock.measured_at = Some(ts(now));
            clock.measured_mono_ms = sf::mono_ms();
            clock.source = Some("hello_ack".into());
            clock.unverified = false;
        }
        self.changed = true;
        restored
    }

    /// 連線中斷（或連線失敗）
    pub fn disconnected(&mut self, d: Disconnected) {
        crate::metrics::global().disconnected(d.code);
        let now = Utc::now();
        if self.outage_started.is_none() {
            self.outage_started = Some((Instant::now(), now));
        }
        let since = self.outage_started.map(|(_, t)| t).unwrap_or(now);
        let c = &mut self.file.connection;
        let state = if d.attention {
            "rejected"
        } else {
            "reconnecting"
        };
        if c.state != state || c.last_error_code.as_deref() != d.code {
            self.changed = true;
        }
        c.state = state.into();
        c.tls_version = None;
        c.since = Some(ts(since));
        c.consecutive_failures = d.consecutive_failures;
        c.next_retry_at = chrono::Duration::from_std(d.retry_in)
            .ok()
            .map(|r| ts(now + r));
        c.last_error_code = d.code.map(str::to_string);
        c.last_error = Some(d.message.clone());
        if d.attention {
            let same = c
                .last_rejection
                .as_ref()
                .is_some_and(|r| r.code.as_deref() == d.code);
            if !same {
                c.last_rejection = Some(sf::Rejection {
                    code: d.code.map(str::to_string),
                    since: ts(now),
                    message: d.message,
                });
            }
        }
        self.connected_durable = None;
        self.retry = None;
    }

    /// 中斷已達 `after` 且這次中斷還沒有記錄過時回傳 `true`（每次中斷只回傳一次）
    pub fn outage_reached(&mut self, after: Duration) -> bool {
        let reached = self
            .outage_started
            .is_some_and(|(at, _)| at.elapsed() >= after);
        if reached && !self.outage_logged {
            self.outage_logged = true;
            self.changed = true;
            return true;
        }
        false
    }

    /// 無法開啟本機緩衝（`PLS-E4004`）
    pub fn buffer_unavailable(&mut self, error: String) {
        self.buffer_error = Some(error);
        self.changed = true;
    }

    /// 停用本機緩衝（`buffer.enabled = false`）
    pub fn buffer_disabled(&mut self) {
        self.buffer_disabled = true;
        self.changed = true;
    }

    /// 本機緩衝有需要立即反映的變化（丟棄資料、寫入失敗、補送完成）
    pub fn buffer_changed(&mut self) {
        self.changed = true;
    }

    /// 記下 agent_id 與主機名稱。未啟用 mTLS 時 agent_id 就是身分憑據（README「agent.id」），
    /// 而狀態檔所有人可讀：不寫入狀態檔，其他欄位中出現的 agent_id 也換成 `<agent_id>`
    /// （呼叫前須已呼叫 [`State::set_client_cert`]）
    pub fn set_identity(&mut self, agent_id: &str, hostname: &str) {
        if self.file.connection.mtls {
            self.file.agent_id = Some(agent_id.to_string());
        } else {
            self.file.agent_id = None;
            self.file.agent_id_hidden = true;
            self.secret_id = Some(agent_id.to_string());
        }
        self.file.hostname = Some(hostname.to_string());
        self.changed = true;
    }

    pub fn add_identity_warning(&mut self, note: CodeNote) {
        if !self.file.identity.warnings.contains(&note) {
            self.file.identity.warnings.push(note);
            self.changed = true;
        }
    }

    pub fn add_config_warning(&mut self, note: CodeNote) {
        if !self.file.config.warnings.contains(&note) {
            self.file.config.warnings.push(note);
            self.changed = true;
        }
    }

    /// 身分與註冊的狀態（`enroll` 模組；設計包 `enrollment-ux.md` §4.2）
    pub fn set_enrollment(
        &mut self,
        state: Option<String>,
        enrollment: Option<sf::EnrollmentStatus>,
        grace: Option<sf::GraceStatus>,
    ) {
        let id = &mut self.file.identity;
        if id.state != state || id.enrollment != enrollment || id.grace != grace {
            id.state = state;
            id.enrollment = enrollment;
            id.grace = grace;
            self.changed = true;
        }
    }

    /// 8443 正常輪替失敗（`PLS-W1111`）
    pub fn renew_failed(&mut self, code: &str) {
        self.file.identity.renew_failed_code = Some(code.to_string());
        self.changed = true;
    }

    /// 下一次輪替的時點
    pub fn set_renew_after(&mut self, at: Option<DateTime<Utc>>) {
        self.file.identity.renew_after = at.map(ts);
    }

    /// 由註冊端點（8444）的回應量測的時鐘偏差（`source` 為 `clock_skew`、`server_info_unverified` 等）
    pub fn clock_from_enroll(&mut self, offset_ms: i64, source: &str) {
        let now = Utc::now();
        let clock = &mut self.file.clock;
        clock.offset_ms = Some(offset_ms);
        clock.measured_at = Some(ts(now));
        clock.measured_mono_ms = sf::mono_ms();
        clock.source = Some(source.to_string());
        clock.unverified = source == "server_info_unverified";
        self.changed = true;
    }

    pub fn set_client_cert(&mut self, info: crate::tls::ClientCertInfo) {
        let id = &mut self.file.identity;
        id.client_cert = Some(info.path);
        id.cert_serial = info.serial;
        id.not_before = info.not_before.map(ts);
        id.not_after = info.not_after.map(ts);
        self.file.connection.mtls = true;
        self.changed = true;
    }

    /// 寫出的內容（緩衝的統計向 `spool` 取得）
    fn snapshot(&mut self, spool: Option<&SharedSpool>, settings: &BufferSettings) -> StatusFile {
        let now = Utc::now();
        let window = self.window();
        let mut file = self.file.clone();
        file.updated_at = ts(now);
        file.mono_ms = sf::mono_ms();
        file.reporting.window_1h = window;
        let r = &mut file.reporting;
        let (delivery, reason) = if self.buffer_disabled {
            ("at_most_once", Some("disabled"))
        } else if self.buffer_error.is_some() || spool.is_none() {
            ("at_most_once", Some("unavailable"))
        } else {
            // 中斷中沿用上一條連線的協商結果；還沒連上過時無法得知
            match self.connected_durable.or(self.last_durable) {
                Some(true) => ("durable", None),
                Some(false) => ("at_most_once", Some("server")),
                None => ("at_most_once", Some("not_connected")),
            }
        };
        r.delivery = delivery.into();
        r.delivery_reason = reason.map(str::to_string);
        r.replay_interval_ms = settings.replay_interval_ms;
        let (buffer, buffered_1h) = buffer_snapshot(
            spool,
            settings,
            self.buffer_disabled,
            self.buffer_error.as_deref(),
            self.connected_durable == Some(true),
            self.retry,
            now,
        );
        r.buffer = buffer;
        r.window_1h.buffered = buffered_1h;
        file
    }
}

/// 本機緩衝的設定（寫進狀態檔，`pulse status` 計算用量與預計補完時間）
#[derive(Debug, Clone, Default)]
pub struct BufferSettings {
    pub dir: Option<PathBuf>,
    pub max_bytes: u64,
    pub max_records: u64,
    pub max_age_hours: u64,
    pub replay_interval_ms: u64,
}

fn opt_ts_ms(ms: Option<i64>) -> Option<String> {
    ms.and_then(DateTime::from_timestamp_millis).map(ts)
}

fn buffer_snapshot(
    spool: Option<&SharedSpool>,
    settings: &BufferSettings,
    disabled: bool,
    error: Option<&str>,
    connected_durable: bool,
    retry: Option<(DateTime<Utc>, &'static str)>,
    now: DateTime<Utc>,
) -> (sf::Buffer, u64) {
    if disabled {
        let buffer = sf::Buffer {
            enabled: false,
            state: "disabled".into(),
            ..sf::Buffer::default()
        };
        return (buffer, 0);
    }
    let dir = settings.dir.as_ref().map(|d| d.display().to_string());
    let Some(spool) = spool else {
        let buffer = sf::Buffer {
            enabled: true,
            state: "unavailable".into(),
            dir,
            error: error.map(str::to_string),
            ..sf::Buffer::default()
        };
        return (buffer, 0);
    };
    let guard = spool.lock();
    let stats = guard.stats();
    let oldest = guard.oldest_sampled_at();
    let buffered_1h =
        guard.pending_sampled_since((now - chrono::Duration::hours(1)).timestamp_millis()) as u64;
    drop(guard);
    let pending = stats.pending as u64;
    let usage = if settings.max_bytes == 0 || settings.max_records == 0 {
        0.0
    } else {
        (stats.pending_bytes as f64 / settings.max_bytes as f64)
            .max(pending as f64 / settings.max_records as f64)
    };
    let last_full = opt_ts_ms(stats.last_dropped_full_at_ms);
    let last_expired = opt_ts_ms(stats.last_dropped_expired_at_ms);
    let last_dropped_at = stats
        .last_dropped_full_at_ms
        .max(stats.last_dropped_expired_at_ms);
    let buffer = sf::Buffer {
        enabled: true,
        state: "ok".into(),
        dir,
        pending: Some(pending),
        pending_bytes: Some(stats.pending_bytes),
        oldest_sample_at: oldest.map(ts),
        max_bytes: Some(settings.max_bytes),
        max_records: Some(settings.max_records),
        max_age_hours: Some(settings.max_age_hours),
        usage_ratio: Some((usage * 1000.0).round() / 1000.0),
        replaying: Some(connected_durable && pending > 0),
        // 由 pulse status 依讀取當下的連線狀態計算
        replay_eta_ms: None,
        dropped_full: Some(stats.dropped_full),
        dropped_expired: Some(stats.dropped_expired),
        dropped_rejected: Some(stats.dropped_rejected),
        write_failures: Some(stats.write_failures),
        last_dropped_at: opt_ts_ms(last_dropped_at),
        counters_since: None,
        last_dropped_full_at: last_full,
        last_dropped_expired_at: last_expired,
        dropped_full_from: opt_ts_ms(stats.dropped_full_from_ms),
        dropped_full_until: opt_ts_ms(stats.dropped_full_until_ms),
        dropped_expired_from: opt_ts_ms(stats.dropped_expired_from_ms),
        dropped_expired_until: opt_ts_ms(stats.dropped_expired_until_ms),
        last_write_failure_at: opt_ts_ms(stats.last_write_failure_at_ms),
        last_write_error: stats.last_write_error.clone(),
        error: None,
        retry_at: retry.filter(|(at, _)| *at > now).map(|(at, _)| ts(at)),
        retry_code: retry
            .filter(|(at, _)| *at > now)
            .map(|(_, c)| c.to_string()),
    };
    (buffer, buffered_1h)
}

/// 追蹤器：狀態、寫出的位置與本機緩衝
/// 寫出的狀態（寫出時持有鎖，寫入依序進行）
#[derive(Debug, Default)]
struct WriteState {
    /// 已寫出結束前的最後一份：之後的定期寫入不再覆寫
    finalized: bool,
    /// 預設位置無法寫入，本次執行改寫在備用位置
    using_fallback: bool,
    /// 已處理過備用位置上遺留的舊檔
    fallback_cleaned: bool,
}

#[derive(Debug)]
pub struct Tracker {
    state: Mutex<State>,
    changed: Notify,
    /// 狀態檔的位置
    path: PathBuf,
    /// 預設位置無法寫入時改寫的位置（`status.file` 未設定時為工作目錄的 `logs/status.json`）
    fallback: Option<PathBuf>,
    spool: Mutex<Option<Arc<SharedSpool>>>,
    buffer: BufferSettings,
    writes: Mutex<WriteState>,
}

impl Tracker {
    pub fn new(
        path: PathBuf,
        fallback: Option<PathBuf>,
        base: StatusFile,
        buffer: BufferSettings,
    ) -> Self {
        Self {
            state: Mutex::new(State::new(base)),
            changed: Notify::new(),
            path,
            fallback,
            spool: Mutex::new(None),
            buffer,
            writes: Mutex::new(WriteState::default()),
        }
    }

    /// 目前寫出的位置（改寫在備用位置時為備用位置）
    pub fn path(&self) -> PathBuf {
        let writes = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        match &self.fallback {
            Some(fb) if writes.using_fallback => fb.clone(),
            _ => self.path.clone(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 更新狀態；有狀態變化時通知寫出任務
    pub fn update<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.lock();
        let out = f(&mut state);
        if state.changed {
            self.changed.notify_one();
        }
        out
    }

    pub fn set_spool(&self, spool: Arc<SharedSpool>) {
        *self.spool.lock().unwrap_or_else(|e| e.into_inner()) = Some(spool);
        self.update(State::buffer_changed);
    }

    /// 目前的內容，但不清除「有變化」的標記（`/metrics` 讀取，不影響狀態檔的寫出時機）
    pub fn peek(&self) -> StatusFile {
        let spool = self.spool.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut state = self.lock();
        let changed = state.changed;
        let file = state.snapshot(spool.as_deref(), &self.buffer);
        state.changed = changed;
        file
    }

    /// 目前的內容
    pub fn snapshot(&self) -> StatusFile {
        let spool = self.spool.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut state = self.lock();
        state.changed = false;
        state.snapshot(spool.as_deref(), &self.buffer)
    }

    /// 寫出一次；回傳寫入結果。已寫出結束前的最後一份時不再寫
    pub fn write(&self) -> std::io::Result<()> {
        let mut writes = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        if writes.finalized {
            return Ok(());
        }
        self.write_locked(&mut writes)
    }

    /// 寫出的內容：未啟用 mTLS 時，任何欄位中出現的 agent_id 都換成 `<agent_id>`
    fn render(&self) -> std::io::Result<Vec<u8>> {
        let file = self.snapshot();
        let mut json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        let secret = self.lock().secret_id.clone();
        if let Some(id) = secret.filter(|id| !id.is_empty()) {
            json = json.replace(&id, "<agent_id>");
        }
        json.push('\n');
        Ok(json.into_bytes())
    }

    fn write_locked(&self, writes: &mut WriteState) -> std::io::Result<()> {
        if !writes.using_fallback {
            match sf::write_atomically(&self.path, &self.render()?) {
                Ok(()) => {
                    // 換上新版 unit 之後：刪除備用位置上的舊檔，避免 pulse status 讀到過時的狀態
                    if !writes.fallback_cleaned {
                        writes.fallback_cleaned = true;
                        if let Some(fb) = &self.fallback {
                            let _ = std::fs::remove_file(fb);
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    let Some(fb) = &self.fallback else {
                        return Err(e);
                    };
                    writes.using_fallback = true;
                    let msg = format!(
                        "無法寫入 {}（{e}），改寫在 {}；一般使用者無法執行 pulse status。\
                         請換上新版 pulse.service（StateDirectory=pulse）後重新啟動",
                        self.path.display(),
                        fb.display()
                    );
                    log::warn!(
                        "{}{msg}",
                        codes::tag(codes::STATUS_WRITE_FAILED, Some(codes::E_STATUS_UNWRITABLE))
                    );
                    self.update(|s| {
                        s.add_config_warning(CodeNote::new(codes::E_STATUS_UNWRITABLE, msg));
                    });
                }
            }
        }
        let fb = self.fallback.as_ref().unwrap_or(&self.path);
        sf::write_atomically(fb, &self.render()?)
    }

    /// 結束前最後一次寫入：記錄結束時間與結束代碼（`pulse status` 顯示「上次結束」）
    pub fn write_final(&self, exit_code: i32, message: Option<&str>) {
        self.update(|s| {
            s.file.running = false;
            s.file.stopped_at = Some(ts(Utc::now()));
            s.file.exit_code = Some(exit_code);
            s.file.exit_message = message.map(|m| m.replace(['\r', '\n'], " "));
            s.file.connection.next_retry_at = None;
        });
        let mut writes = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = self.write_locked(&mut writes) {
            log::debug!("結束時無法寫入狀態檔 {}：{e}", self.path.display());
        }
        writes.finalized = true;
    }
}

static TRACKER: OnceLock<Arc<Tracker>> = OnceLock::new();

/// 常駐模式啟動時初始化（只有第一次有效）
pub fn init(tracker: Tracker) -> Arc<Tracker> {
    TRACKER.get_or_init(|| Arc::new(tracker)).clone()
}

/// 已初始化的追蹤器
pub fn tracker() -> Option<&'static Arc<Tracker>> {
    TRACKER.get()
}

/// 更新狀態；尚未初始化（單元測試、其他子命令）時什麼都不做
pub fn record(f: impl FnOnce(&mut State)) {
    if let Some(t) = TRACKER.get() {
        t.update(f);
    }
}

/// 啟動時的基本內容：版本、行程、設定檔與「最近 1 小時的啟動時間」
/// （沿用上一個行程留下的狀態檔，用來計算最近 1 小時重啟次數）
pub fn initial_file(
    previous: Option<&StatusFile>,
    config_file: Option<String>,
    working_dir: Option<String>,
    report_interval: Duration,
    server: String,
    display_name: Option<String>,
) -> StatusFile {
    let now = Utc::now();
    let hour_ago = now - chrono::Duration::hours(1);
    let mut starts: Vec<String> = previous
        .map(|p| p.starts_1h.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|s| sf::parse_ts(s).is_some_and(|t| t > hour_ago && t <= now))
        .collect();
    starts.push(ts(now));
    StatusFile {
        schema: sf::SCHEMA.into(),
        pid: std::process::id(),
        started_at: ts(now),
        starts_1h: starts,
        running: true,
        version: env!("CARGO_PKG_VERSION").into(),
        commit: env!("PULSE_BUILD_COMMIT").into(),
        working_dir,
        config_file,
        display_name,
        service: detect_service(),
        report_interval_ms: u64::try_from(report_interval.as_millis()).unwrap_or(u64::MAX),
        connection: sf::Connection {
            state: "connecting".into(),
            server,
            ..sf::Connection::default()
        },
        ..StatusFile::default()
    }
}

/// 偵測是否由 systemd 啟動（`/proc/self/cgroup` 中的 `.service`）；Windows 與其他情況為未知
fn detect_service() -> sf::ServiceHint {
    #[cfg(target_os = "linux")]
    if std::env::var_os("INVOCATION_ID").is_some()
        && let Ok(text) = std::fs::read_to_string("/proc/self/cgroup")
        && let Some(unit) = unit_from_cgroup(&text)
    {
        return sf::ServiceHint {
            manager: Some("systemd".into()),
            unit: Some(unit),
        };
    }
    sf::ServiceHint::default()
}

/// `/proc/self/cgroup` 的內容中，最後一段以 `.service` 結尾的路徑元件
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unit_from_cgroup(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .filter_map(|l| l.rsplit(':').next())
        .find_map(|path| {
            path.rsplit('/').find(|c| {
                c.ends_with(".service")
                    && c.len() <= 256
                    && c.chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "@._-:\\".contains(ch))
            })
        })
        .map(str::to_string)
}

/// 定期與狀態變化時寫出狀態檔，直到 `shutdown`。寫入失敗依 [`WriteFailures`] 記錄（`PLS-E8204`），
/// 恢復時記 `status.write_restored`。
pub async fn run_writer(tracker: Arc<Tracker>, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(sf::WRITE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_write: Option<tokio::time::Instant> = None;
    let mut failures = WriteFailures::default();
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = tracker.changed.notified() => {
                if let Some(last) = last_write {
                    tokio::select! {
                        _ = tokio::time::sleep_until(last + sf::MIN_WRITE_GAP) => {}
                        _ = shutdown.cancelled() => return,
                    }
                }
            }
            _ = shutdown.cancelled() => return,
        }
        let now = tokio::time::Instant::now();
        last_write = Some(now);
        match tracker.write() {
            Ok(()) => {
                if let Some(failing) = failures.succeeded(now) {
                    log::info!(
                        "{}已恢復寫入狀態檔 {}（連續失敗約 {} 秒）",
                        codes::tag(codes::STATUS_WRITE_RESTORED, None),
                        tracker.path().display(),
                        failing.as_secs()
                    );
                }
            }
            Err(e) => {
                let tag = codes::tag(codes::STATUS_WRITE_FAILED, Some(codes::E_STATUS_UNWRITABLE));
                let path = tracker.path();
                let path = path.display();
                match failures.failed(now) {
                    WriteFailureLog::Warn => log::warn!(
                        "{tag}無法寫入狀態檔 {path}：{e}；監控與回報照常，連續失敗 60 秒後 pulse status 會顯示\
                         「未知」。請確認目錄可由服務帳號寫入（或以 status.file 指定其他位置）"
                    ),
                    WriteFailureLog::Error {
                        suppressed,
                        failing,
                    } => {
                        let more = if suppressed > 0 {
                            format!("（上一則之後另有 {suppressed} 次失敗只記 DEBUG）")
                        } else {
                            String::new()
                        };
                        let msg = format!(
                            "{tag}已連續約 {} 秒無法寫入狀態檔 {path}：{e}；pulse status 與「Pulse 狀態」\
                             視窗顯示「未知」。請確認目錄可由服務帳號寫入、磁碟有空間\
                             （或以 status.file 指定其他位置）{more}",
                            failing.as_secs()
                        );
                        log::error!("{msg}");
                        crate::service_status::attention(&msg);
                    }
                    WriteFailureLog::Debug => {
                        log::debug!("{tag}無法寫入狀態檔 {path}：{e}")
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn status_write_failures_warn_then_escalate_and_restore() {
        // log-ux.md §4.5：第一次 WARN、連續失敗滿 60 秒 ERROR、之後每 10 分鐘一則 ERROR 摘要、
        // 其間 DEBUG；恢復時回報失敗了多久（status.write_restored）
        let t0 = tokio::time::Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut f = WriteFailures::default();
        assert_eq!(f.succeeded(at(0)), None, "沒有失敗過時不記恢復");
        assert_eq!(f.failed(at(0)), WriteFailureLog::Warn);
        for s in (10..60).step_by(10) {
            assert_eq!(f.failed(at(s)), WriteFailureLog::Debug, "{s} 秒");
        }
        assert_eq!(
            f.failed(at(60)),
            WriteFailureLog::Error {
                suppressed: 5,
                failing: Duration::from_secs(60)
            }
        );
        assert_eq!(f.failed(at(70)), WriteFailureLog::Debug);
        assert_eq!(f.failed(at(659)), WriteFailureLog::Debug);
        assert_eq!(
            f.failed(at(660)),
            WriteFailureLog::Error {
                suppressed: 2,
                failing: Duration::from_secs(660)
            }
        );
        assert_eq!(f.succeeded(at(665)), Some(Duration::from_secs(665)));
        // 下一段失敗重新從 WARN 開始
        assert_eq!(f.failed(at(700)), WriteFailureLog::Warn);
        assert_eq!(f.failed(at(759)), WriteFailureLog::Debug);
        assert!(matches!(f.failed(at(760)), WriteFailureLog::Error { .. }));
    }

    fn tracker(dir: &Path) -> Tracker {
        Tracker::new(
            dir.join("status.json"),
            None,
            initial_file(
                None,
                Some("/opt/pulse/pulse.toml".into()),
                Some("/opt/pulse".into()),
                Duration::from_secs(5),
                "kairos.example.gov.tw:8443".into(),
                None,
            ),
            BufferSettings::default(),
        )
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pulse-local-status-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn connection_lifecycle_is_recorded() {
        let dir = temp_dir();
        let t = tracker(&dir);
        assert_eq!(t.snapshot().connection.state, "connecting");
        assert_eq!(
            t.snapshot().reporting.delivery_reason.as_deref(),
            Some("unavailable"),
            "沒有緩衝"
        );

        t.update(|s| {
            s.disconnected(Disconnected {
                code: Some(codes::E_CONN_REFUSED),
                message: "TCP 連線失敗：Connection refused".into(),
                attention: false,
                consecutive_failures: 3,
                retry_in: Duration::from_secs(52),
            })
        });
        let f = t.snapshot();
        assert_eq!(f.connection.state, "reconnecting");
        assert_eq!(f.connection.last_error_code.as_deref(), Some("PLS-E2003"));
        assert_eq!(f.connection.consecutive_failures, 3);
        assert!(f.connection.next_retry_at.is_some());
        assert!(f.connection.last_rejection.is_none());

        // 需要人工處理的拒絕：保留到下次成功註冊；之後的一般網路錯誤不會蓋掉
        t.update(|s| {
            s.disconnected(Disconnected {
                code: Some(codes::E_CLIENT_CERT_REJECTED),
                message: "certificate_rejected".into(),
                attention: true,
                consecutive_failures: 4,
                retry_in: Duration::from_secs(2),
            })
        });
        t.update(|s| {
            s.disconnected(Disconnected {
                code: Some(codes::E_CONN_REFUSED),
                message: "refused".into(),
                attention: false,
                consecutive_failures: 5,
                retry_in: Duration::from_secs(2),
            })
        });
        let f = t.snapshot();
        assert_eq!(
            f.connection
                .last_rejection
                .as_ref()
                .unwrap()
                .code
                .as_deref(),
            Some("PLS-E1105")
        );
        let since = f.connection.since.clone();

        let restored = t.update(|s| {
            s.registered(Registered {
                tls_version: Some("TLSv1.3".into()),
                server_verified: true,
                proto: 2,
                features: vec!["metrics".into(), "durable".into()],
                durable: true,
                server_time: Some(Utc::now() - chrono::Duration::milliseconds(1500)),
            })
        });
        assert_eq!(restored.unwrap().attempts, 5);
        let f = t.snapshot();
        assert_eq!(f.connection.state, "registered");
        assert!(f.connection.last_rejection.is_none());
        assert!(since.is_some());
        assert!(
            f.connection.last_outage_from.is_some() && f.connection.last_outage_until.is_some()
        );
        let offset = f.clock.offset_ms.unwrap();
        assert!((1400..5000).contains(&offset), "{offset}");
        assert_eq!(f.clock.source.as_deref(), Some("hello_ack"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn outage_is_reported_once_per_outage() {
        let dir = temp_dir();
        let t = tracker(&dir);
        let lost = || Disconnected {
            code: None,
            message: "x".into(),
            attention: false,
            consecutive_failures: 1,
            retry_in: Duration::ZERO,
        };
        t.update(|s| s.disconnected(lost()));
        assert!(t.update(|s| s.outage_reached(Duration::ZERO)));
        assert!(!t.update(|s| s.outage_reached(Duration::ZERO)));
        t.update(|s| {
            s.registered(Registered {
                tls_version: None,
                server_verified: true,
                proto: 1,
                features: vec![],
                durable: false,
                server_time: None,
            })
        });
        assert!(!t.update(|s| s.outage_reached(Duration::ZERO)), "已恢復");
        t.update(|s| s.disconnected(lost()));
        assert!(t.update(|s| s.outage_reached(Duration::ZERO)), "新的中斷");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn window_counts_the_last_hour() {
        let dir = temp_dir();
        let t = tracker(&dir);
        t.update(|s| {
            for _ in 0..3 {
                s.sampled();
            }
            s.report_sent();
            s.report_acked();
            s.report_rejected();
            s.dropped_disconnected();
        });
        let f = t.snapshot();
        assert_eq!(
            f.reporting.window_1h,
            sf::Window1h {
                sampled: 3,
                acked: 1,
                buffered: 0,
                dropped_disconnected: 1,
                rejected_by_server: 1,
            }
        );
        assert!(f.reporting.last_report_at.is_some() && f.reporting.last_ack_at.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_and_final_write_are_readable() {
        let dir = temp_dir();
        let t = tracker(&dir);
        t.write().unwrap();
        let f = sf::read(&t.path()).unwrap();
        assert!(f.running);
        assert_eq!(f.pid, std::process::id());
        assert_eq!(f.starts_1h.len(), 1);
        t.write_final(3, Some("設定錯誤\n第 4 行"));
        let f = sf::read(&t.path()).unwrap();
        assert!(!f.running);
        assert_eq!(f.exit_code, Some(3));
        assert_eq!(f.exit_message.as_deref(), Some("設定錯誤 第 4 行"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unwritable_default_location_falls_back_to_logs() {
        // 沿用舊 unit（/var/lib/pulse 不可寫）：改寫在 logs/status.json，服務照常，並記 PLS-E8204
        let dir = temp_dir();
        std::fs::write(dir.join("not-a-dir"), b"x").unwrap();
        let fallback = dir.join("logs").join("status.json");
        let t = Tracker::new(
            dir.join("not-a-dir").join("status.json"),
            Some(fallback.clone()),
            initial_file(None, None, None, Duration::from_secs(5), "a:1".into(), None),
            BufferSettings::default(),
        );
        t.write().unwrap();
        assert_eq!(t.path(), fallback);
        let f = sf::read(&fallback).unwrap();
        assert_eq!(f.config.warnings.len(), 1, "{:?}", f.config.warnings);
        assert_eq!(f.config.warnings[0].code, "PLS-E8204");
        assert!(
            f.config.warnings[0]
                .message
                .contains("StateDirectory=pulse")
        );
        // 之後的寫入直接寫備用位置，不重複警告
        t.write().unwrap();
        assert_eq!(sf::read(&fallback).unwrap().config.warnings.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writable_default_location_removes_the_old_fallback_file() {
        // 換上新版 unit 後：備用位置留下的舊檔刪除，pulse status 不會讀到過時的狀態
        let dir = temp_dir();
        let fallback = dir.join("logs").join("status.json");
        std::fs::create_dir_all(fallback.parent().unwrap()).unwrap();
        std::fs::write(&fallback, b"{}").unwrap();
        let primary = dir.join("state").join("status.json");
        let t = Tracker::new(
            primary.clone(),
            Some(fallback.clone()),
            initial_file(None, None, None, Duration::from_secs(5), "a:1".into(), None),
            BufferSettings::default(),
        );
        t.write().unwrap();
        assert_eq!(t.path(), primary);
        assert!(sf::read(&primary).unwrap().config.warnings.is_empty());
        assert!(!fallback.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn agent_id_is_not_written_without_mtls() {
        // 未啟用 mTLS 時 agent_id 等同身分憑據，狀態檔所有人可讀：任何欄位都不出現
        const ID: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
        let dir = temp_dir();
        let t = tracker(&dir);
        t.update(|s| s.set_identity(ID, "web-01"));
        t.update(|s| {
            s.disconnected(Disconnected {
                code: Some(codes::E_AGENT_ID_CONFLICT),
                message: format!("superseded：agent_id {ID} 被取代"),
                attention: true,
                consecutive_failures: 2,
                retry_in: Duration::from_secs(30),
            })
        });
        t.write().unwrap();
        let text = std::fs::read_to_string(t.path()).unwrap();
        assert!(!text.contains(ID), "{text}");
        let f = sf::read(&t.path()).unwrap();
        assert_eq!(f.agent_id, None);
        assert!(f.agent_id_hidden);
        assert!(f.connection.last_error.unwrap().contains("<agent_id>"));
        assert_eq!(f.hostname.as_deref(), Some("web-01"));

        // 啟用 mTLS（身分由憑證與私鑰保護）：照常寫入
        let t = tracker(&dir);
        t.update(|s| {
            s.set_client_cert(crate::tls::ClientCertInfo {
                path: "client.pem".into(),
                serial: Some("10:02".into()),
                not_before: None,
                not_after: None,
            })
        });
        t.update(|s| s.set_identity(ID, "web-01"));
        t.write().unwrap();
        let f = sf::read(&t.path()).unwrap();
        assert_eq!(f.agent_id.as_deref(), Some(ID));
        assert!(!f.agent_id_hidden);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restarts_within_the_last_hour_are_carried_over() {
        let now = Utc::now();
        let previous = StatusFile {
            starts_1h: vec![
                ts(now - chrono::Duration::minutes(90)),
                ts(now - chrono::Duration::minutes(30)),
                ts(now - chrono::Duration::minutes(5)),
                "garbage".into(),
                ts(now + chrono::Duration::hours(2)),
            ],
            ..StatusFile::default()
        };
        let f = initial_file(
            Some(&previous),
            None,
            None,
            Duration::from_secs(5),
            "a:1".into(),
            None,
        );
        // 90 分鐘前、無法解析與未來的時間都不算；加上這次
        assert_eq!(f.starts_1h.len(), 3, "{:?}", f.starts_1h);
    }

    #[test]
    fn systemd_unit_is_read_from_the_cgroup_path() {
        assert_eq!(
            unit_from_cgroup("0::/system.slice/pulse.service\n").as_deref(),
            Some("pulse.service")
        );
        assert_eq!(
            unit_from_cgroup("12:pids:/system.slice/pulse-2.service\n1:name=systemd:/x\n")
                .as_deref(),
            Some("pulse-2.service")
        );
        assert_eq!(
            unit_from_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n"),
            None
        );
    }

    #[test]
    fn delivery_reflects_buffer_and_negotiation() {
        let dir = temp_dir();
        let t = tracker(&dir);
        t.update(State::buffer_disabled);
        let f = t.snapshot();
        assert_eq!(
            (
                f.reporting.delivery.as_str(),
                f.reporting.delivery_reason.as_deref()
            ),
            ("at_most_once", Some("disabled"))
        );
        assert_eq!(f.reporting.buffer.state, "disabled");
        assert_eq!(f.reporting.buffer.pending, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
