//! 告警引擎（審計 F3；設計包 `kairos/screens.md` §24 #9、`kairos/alerts-ux.md`；規格見 `kairos/ALERTS.md`）。
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`model`] | 嚴重度、處理類型、狀態、規則、事件、靜音、通知管道 |
//! | [`rules`] | 系統預設規則與使用者規則的驗證（`KRS-E5001`） |
//! | [`scope`] | 範圍的比對（同欄位「或」、跨欄位「且」，D23）與臨時靜音的時長上限（D26） |
//! | [`engine`]、[`eval`] | 狀態機：準位觸發、去重與重新開啟、抖動、關聯彙總、暖機期、靜音、重送與升級 |
//! | [`probe`] | 系統檢查（CRL、伺服器憑證、寫入、容量、連線上限、註冊端點…） |
//! | [`store`] | 資料表（schema v5）、保留期限與筆數上限 |
//! | [`notify`] | 通知的範本、Webhook 與 SMTP、派送（速率、合併、重試、退避、管道失敗） |
//! | [`secrets`] | 通知管道機密的加密與遮蔽 |
//! | [`backtest`] | 規則的歷史試算 |
//!
//! 引擎在自己的執行緒（current-thread 的 tokio runtime）上執行，與 agent 連線、report 的寫入分開排程；
//! 讀取狀態表只複製參考計數，寫入資料庫以短交易進行（`EnrollDb` 的寫入間隔），不拖慢 report 的 `ack`
//! （量測見 ALERTS.md 第 9 節）。

pub mod backtest;
pub mod engine;
mod eval;
pub mod model;
pub mod notify;
pub mod ops;
pub mod probe;
pub mod rules;
pub mod scope;
pub mod secrets;
pub mod store;
pub mod test_limit;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;

use chrono::Utc;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

use crate::bus::{DisconnectReason, peer_ip};
use crate::config::{AlertSettings, AppConfig};
use crate::enroll::db::{DbError, EnrollDb};
use crate::per_ip::PerIpLimiter;
use crate::registry::Registry;
use crate::shutdown::Shutdown;
use crate::stats::WriteStats;
use crate::tls::TlsState;
use engine::{
    ActionError, Change, Engine, EngineConfig, HostView, Loaded, Output, Rejection, Warmup, World,
};
use model::{Channel, RootCause, Scope};
use notify::{DispatchConfig, Dispatcher};
use probe::{DbFacts, Probe};
use secrets::SecretBox;

/// 判定的間隔
pub const TICK: Duration = Duration::from_secs(5);
/// 清冊與資料庫數值的讀取間隔
pub const INVENTORY_EVERY_TICKS: u64 = 6;
/// 心跳（停機期間的判斷）的寫入間隔
pub const HEARTBEAT_EVERY_TICKS: u64 = 6;
/// 保留期限與筆數上限的清除間隔
pub const PURGE_EVERY_TICKS: u64 = 720;
/// SSE 的變化緩衝
pub const CHANGES_CAPACITY: usize = 1024;
/// 寫入事件時每個交易最多的事件數
pub const FLUSH_CHUNK: usize = 500;

/// 啟動參數
pub struct StartParams {
    pub config: Arc<AppConfig>,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub registry: Arc<Registry>,
    pub write_stats: Option<Arc<WriteStats>>,
    pub per_ip: Option<Arc<PerIpLimiter>>,
    pub tls: Option<Arc<TlsState>>,
    pub enroll: Option<Arc<crate::enroll::EnrollService>>,
    pub agent_listen: Option<SocketAddr>,
    pub shutdown: Arc<Shutdown>,
    /// 設定檔沒有 `[enroll]` 的 mode（`KRS-I2010`）
    pub enroll_closed_default: bool,
    pub clock: fn() -> i64,
}

/// 清冊中的一台主機（每 30 秒讀一次）
#[derive(Debug, Clone)]
struct InvHost {
    agent_id: String,
    hostname: String,
    display_name: Option<String>,
    tags: Vec<String>,
    peer: Option<String>,
    decommissioned: bool,
    last_seen: i64,
    connected_at: Option<i64>,
    offline_after_ms: Option<i64>,
    report_interval_ms: Option<i64>,
}

/// 告警服務：引擎、派送與 API 的入口
/// [`AlertService::metrics`] 的結果
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    /// 判定週期在 60 秒內執行過
    pub running: bool,
    /// 最近一個判定週期（Unix 毫秒；還沒有時為 0）
    pub last_tick_ms: i64,
    pub tick_seconds: f64,
    /// 告警暖機期中
    pub warmup: bool,
    /// 未解決的事件：[嚴重度（emergency、critical、warning、info）][狀態（firing、acknowledged、recovered）]
    pub open: [[u64; 3]; 4],
    /// 通知管道：[ok、failed、disabled]
    pub channels: [u64; 3],
}

pub struct AlertService {
    pub settings: AlertSettings,
    pub db: Arc<EnrollDb>,
    pub secrets: Arc<SecretBox>,
    pub dispatcher: Arc<Dispatcher>,
    engine: tokio::sync::Mutex<Engine>,
    changes: broadcast::Sender<Change>,
    probe: Mutex<Probe>,
    registry: Arc<Registry>,
    clock: fn() -> i64,
    retry: Mutex<Output>,
    config_rev: AtomicI64,
    reload: AtomicBool,
    last_tick: AtomicI64,
    /// 最近一個判定週期花的時間（微秒；狀態與量測用）
    tick_micros: AtomicI64,
    last_error: Mutex<Option<String>>,
    inventory: Mutex<Arc<Vec<InvHost>>>,
    revoked: Mutex<Arc<HashSet<String>>>,
    db_facts: Mutex<Option<DbFacts>>,
    offline_since: Mutex<HashMap<String, i64>>,
    pub started_ms: i64,
    stopped: AtomicBool,
    /// 機密無法解開的管道（狀態轉換時記 WARN 並發布內部事件，[`AlertService::watch_secrets`]）
    secrets_missing: Mutex<HashSet<String>>,
    /// 「寄送測試通知」的限速（每管道、每使用者）
    test_limits: Mutex<test_limit::TestLimiter>,
}

fn db_err(e: DbError) -> String {
    e.to_string()
}

fn read_inventory(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<InvHost>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, hostname, display_name, tags, peer, lifecycle, last_seen, connected_at,
                offline_after_ms, report_interval_ms FROM hosts",
    )?;
    let rows = stmt.query_map([], |r| {
        let tags: Option<String> = r.get(3)?;
        Ok(InvHost {
            agent_id: r.get(0)?,
            hostname: r.get(1)?,
            display_name: r.get(2)?,
            tags: tags
                .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.to_lowercase())
                .collect(),
            peer: r.get(4)?,
            decommissioned: r.get::<_, String>(5)? != "active",
            last_seen: r.get(6)?,
            connected_at: r.get(7)?,
            offline_after_ms: r.get(8)?,
            report_interval_ms: r.get(9)?,
        })
    })?;
    rows.collect()
}

fn read_db_facts(conn: &rusqlite::Connection) -> rusqlite::Result<(DbFacts, HashSet<String>)> {
    let capacity = store::get_meta(conn, crate::store::sqlite::META_CAPACITY_EVENT)?
        .and_then(|s| serde_json::from_str(&s).ok());
    let guard: Option<date_guard::GuardState> =
        store::get_meta(conn, "retention_guard")?.and_then(|s| serde_json::from_str(&s).ok());
    let guard_paused =
        guard.is_some_and(|g| g.jump_since_unix.is_some() || g.rollback_since_unix.is_some());
    let mut stmt =
        conn.prepare("SELECT serial FROM pki_certificates WHERE revoked_at IS NOT NULL")?;
    let revoked = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<String>>>()?;
    Ok((
        DbFacts {
            capacity,
            guard_paused,
        },
        revoked,
    ))
}

/// 心跳：（時間, 是否正常關機）
fn parse_heartbeat(text: Option<String>) -> Option<(i64, bool)> {
    let v: Value = serde_json::from_str(&text?).ok()?;
    Some((v.get("at")?.as_i64()?, v.get("clean")?.as_bool()?))
}

fn days_around(now: i64, offset: chrono::FixedOffset) -> Vec<String> {
    (-2..=1)
        .filter_map(|d| chrono::DateTime::from_timestamp_millis(now + d * 86_400_000))
        .map(|t| t.with_timezone(&offset).format("%Y%m%d").to_string())
        .collect()
}

impl AlertService {
    /// 啟動：載入規則、事件、靜音、管道，記錄重新啟動，並在自己的執行緒上開始判定與派送。
    /// 回傳服務與執行緒（關機時 [`AlertService::join`]）
    pub async fn start(p: StartParams) -> Result<(Arc<Self>, std::thread::JoinHandle<()>), String> {
        let settings = p.config.alerts.clone();
        let db = EnrollDb::new(p.db_path.clone());
        let secrets = Arc::new(SecretBox::open(&p.data_dir).map_err(|e| e.to_string())?);
        let now = (p.clock)();
        let offset = settings.offset();
        let days = days_around(now, offset);
        let sb = secrets.clone();
        let (loaded, previous, expected) = db
            .write(move |conn| {
                store::ensure_builtin_rules(conn, now)?;
                let previous = parse_heartbeat(store::get_meta(conn, store::META_HEARTBEAT)?);
                let mut loaded = Loaded {
                    rules: store::load_rules(conn)?,
                    silences: store::load_silences(conn, now - 86_400_000)?,
                    channels: store::load_channels(conn, Some(sb.as_ref()))?,
                    open_events: store::load_open_events(conn)?,
                    suppressed: store::load_suppressed(conn, now - 30 * 86_400_000)?,
                    seq_by_day: store::load_seq(conn, &days)?,
                };
                loaded.silences.sort_by_key(|s| s.starts_at);
                // 重新啟動前「在線上」的主機：清冊的 last_seen 在上次心跳前 2 分鐘之內
                let expected: HashSet<String> = match previous {
                    Some((at, _)) => {
                        let mut stmt = conn.prepare(
                            "SELECT agent_id FROM hosts WHERE lifecycle = 'active' AND last_seen >= ?1",
                        )?;
                        stmt.query_map([at - engine::PREVIOUS_ONLINE_SLACK_MS], |r| r.get::<_, String>(0))?
                            .collect::<rusqlite::Result<_>>()?
                    }
                    None => HashSet::new(),
                };
                store::put_meta(
                    conn,
                    store::META_HEARTBEAT,
                    &json!({ "at": now, "clean": false }).to_string(),
                )?;
                let rev = store::config_rev(conn)?;
                Ok(((loaded, rev), previous, expected))
            })
            .await
            .map_err(db_err)?;
        let ((loaded, rev), previous) = (loaded, previous);
        let in_maintenance = previous.is_some_and(|(at, _)| {
            loaded.silences.iter().any(|s| {
                s.kind == model::SilenceKind::Maintenance
                    && s.starts_at <= at
                    && at < s.effective_end()
            })
        });
        let n_expected = expected.len();
        let mut engine = Engine::new(
            EngineConfig {
                offset,
                prefix_v4: settings.subnet_prefix_v4,
                prefix_v6: settings.subnet_prefix_v6,
                console_url: settings.console_url.clone(),
            },
            now,
            Warmup {
                until_ms: now + settings.warmup_secs as i64 * 1000,
                expected,
                ended: false,
                missing: HashSet::new(),
            },
            loaded,
        );
        engine.record_startup(previous, p.enroll_closed_default, in_maintenance, now);
        let dispatcher = Dispatcher::new(
            db.clone(),
            secrets.clone(),
            DispatchConfig {
                offset,
                console_url: settings.console_url.clone(),
            },
        );
        let mut probe = Probe::new(p.config.clone(), p.registry.clone());
        probe.write_stats = p.write_stats;
        probe.per_ip = p.per_ip;
        probe.tls = p.tls;
        probe.enroll = p.enroll;
        probe.agent_listen = p.agent_listen;
        let svc = Arc::new(Self {
            settings,
            db,
            secrets,
            dispatcher,
            engine: tokio::sync::Mutex::new(engine),
            changes: broadcast::channel(CHANGES_CAPACITY).0,
            probe: Mutex::new(probe),
            registry: p.registry.clone(),
            clock: p.clock,
            retry: Mutex::new(Output::default()),
            config_rev: AtomicI64::new(rev),
            reload: AtomicBool::new(false),
            last_tick: AtomicI64::new(0),
            tick_micros: AtomicI64::new(0),
            last_error: Mutex::new(None),
            inventory: Mutex::new(Arc::new(Vec::new())),
            revoked: Mutex::new(Arc::new(HashSet::new())),
            db_facts: Mutex::new(None),
            offline_since: Mutex::new(HashMap::new()),
            started_ms: now,
            stopped: AtomicBool::new(false),
            secrets_missing: Mutex::new(HashSet::new()),
            test_limits: Mutex::new(test_limit::TestLimiter::default()),
        });
        // 啟動時的事件（重新啟動、停機）先寫入
        let (out, n_rules, n_open) = {
            let mut engine = svc.engine.lock().await;
            let out = engine.take_output();
            (out, engine.rules().len(), engine.open_events().count())
        };
        svc.flush(out).await;
        log::info!(
            event = "alerts.engine.started";
            "告警引擎已啟動：規則 {n_rules} 條、進行中的事件 {n_open} 個、暖機期 {} 秒（重新啟動前在線上 {n_expected} 台）",
            svc.settings.warmup_secs
        );
        let run_svc = svc.clone();
        let shutdown = p.shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("kairos-alerts".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .max_blocking_threads(4)
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::error!(event = "alerts.engine.failed"; "告警引擎無法建立 runtime：{e}");
                        return;
                    }
                };
                rt.block_on(run_svc.run(shutdown));
                rt.shutdown_timeout(Duration::from_secs(2));
            })
            .map_err(|e| format!("無法建立告警引擎的執行緒：{e}"))?;
        Ok((svc, thread))
    }

    /// 告警引擎的主迴圈
    async fn run(self: Arc<Self>, shutdown: Arc<Shutdown>) {
        let mut bus = self.registry.bus().subscribe();
        let dispatcher = self.dispatcher.clone();
        let dshut = shutdown.clone();
        let clock = self.clock;
        let dispatch = tokio::spawn(async move { dispatcher.run(dshut, clock).await });
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut n: u64 = 0;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            // 匯流排的事件
            {
                let mut engine = self.engine.lock().await;
                loop {
                    match bus.try_recv() {
                        Ok(ev) => engine.on_bus(&ev),
                        Err(broadcast::error::TryRecvError::Lagged(k)) => {
                            self.registry.bus().note_lagged(k);
                        }
                        Err(_) => break,
                    }
                }
            }
            if n.is_multiple_of(2) {
                self.process_actions().await;
            }
            if n.is_multiple_of(TICK.as_secs()) {
                self.maybe_reload().await;
                let tick = n / TICK.as_secs();
                if tick.is_multiple_of(INVENTORY_EVERY_TICKS) {
                    self.refresh_inventory().await;
                }
                self.tick().await;
                if tick.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
                    self.heartbeat(false).await;
                }
                if tick % PURGE_EVERY_TICKS == 12 {
                    self.purge().await;
                }
            }
            n = n.wrapping_add(1);
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), dispatch).await;
        let out = self.engine.lock().await.take_output();
        self.flush(out).await;
        self.heartbeat(true).await;
        self.stopped.store(true, Ordering::SeqCst);
        log::info!(event = "alerts.engine.stopped"; "告警引擎已停止");
    }

    async fn heartbeat(&self, clean: bool) {
        let now = (self.clock)();
        let text = json!({ "at": now, "clean": clean }).to_string();
        if let Err(e) = self
            .db
            .write(move |conn| Ok(store::put_meta(conn, store::META_HEARTBEAT, &text)?))
            .await
        {
            log::debug!("告警引擎的心跳無法寫入：{e}");
        }
    }

    /// 清冊與資料庫的數值（每 30 秒）
    async fn refresh_inventory(&self) {
        let r = self
            .db
            .read(|conn| Ok((read_inventory(conn)?, read_db_facts(conn)?)))
            .await;
        match r {
            Ok((inv, (facts, revoked))) => {
                *self.inventory.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(inv);
                *self.revoked.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(revoked);
                *self.db_facts.lock().unwrap_or_else(|e| e.into_inner()) = Some(facts);
            }
            Err(e) => self.note_error(format!("無法讀取主機清冊：{e}")),
        }
    }

    fn note_error(&self, e: String) {
        log::warn!(event = "alerts.engine.error"; "告警引擎：{e}");
        *self.last_error.lock().unwrap_or_else(|x| x.into_inner()) = Some(e);
    }

    /// 合併清冊與狀態表
    fn world_hosts(&self, now: i64) -> Vec<HostView> {
        let inv = self
            .inventory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let live = self.registry.live_view();
        let mut since = self.offline_since.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: HashMap<String, HostView> = HashMap::with_capacity(inv.len().max(live.len()));
        for h in inv.iter() {
            out.insert(
                h.agent_id.clone(),
                HostView {
                    agent_id: Arc::from(h.agent_id.as_str()),
                    hostname: Arc::from(h.hostname.as_str()),
                    display_name: h.display_name.as_deref().map(Arc::from),
                    tags: h.tags.clone(),
                    ip: h.peer.as_deref().and_then(peer_ip),
                    online: false,
                    offline_at: Some(h.last_seen),
                    offline_reason: None,
                    seen_since_start: false,
                    decommissioned: h.decommissioned,
                    connected_at: h.connected_at,
                    last_seen: h.last_seen,
                    offline_after_ms: h.offline_after_ms.unwrap_or(0).max(0) as u64,
                    report_interval_ms: h.report_interval_ms.map(|v| v.max(0) as u64),
                    last_report_at: None,
                    cpu: None,
                    memory: None,
                    detail: None,
                    logical_cores: None,
                },
            );
        }
        let mut live_ids = HashSet::with_capacity(live.len());
        for l in &live {
            live_ids.insert(l.agent_id.to_string());
            let entry = out
                .entry(l.agent_id.to_string())
                .or_insert_with(|| HostView {
                    agent_id: l.agent_id.clone(),
                    hostname: l.hostname.clone(),
                    display_name: None,
                    tags: Vec::new(),
                    ip: None,
                    online: false,
                    offline_at: None,
                    offline_reason: None,
                    seen_since_start: false,
                    decommissioned: false,
                    connected_at: None,
                    last_seen: 0,
                    offline_after_ms: 0,
                    report_interval_ms: None,
                    last_report_at: None,
                    cpu: None,
                    memory: None,
                    detail: None,
                    logical_cores: None,
                });
            entry.hostname = l.hostname.clone();
            if let Some(info) = &l.host {
                if let Some(d) = &info.display_name {
                    entry.display_name = Some(Arc::from(d.as_str()));
                }
                if let Some(t) = &info.tags {
                    entry.tags = t.iter().map(|t| t.to_lowercase()).collect();
                }
                if let Some(r) = info.report_interval_ms {
                    entry.report_interval_ms = Some(r);
                }
            }
            if let Some(ip) = peer_ip(&l.peer) {
                entry.ip = Some(ip);
            }
            entry.online = l.online;
            entry.seen_since_start = l.seen_since_start;
            entry.connected_at = Some(l.connected_at.timestamp_millis());
            entry.last_seen = l.last_seen.timestamp_millis();
            entry.offline_after_ms = l.offline_after.as_millis() as u64;
            entry.offline_reason = l.offline_reason.map(DisconnectReason::from_offline);
            if let Some(r) = &l.last_report {
                entry.last_report_at = chrono::DateTime::parse_from_rfc3339(&r.recv_at)
                    .ok()
                    .map(|t| t.timestamp_millis());
                entry.cpu = r.cpu_usage.map(f64::from);
                entry.memory = match (r.used_memory, r.total_memory) {
                    (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
                    _ => None,
                };
                entry.detail = Some(r.clone());
            }
            entry.logical_cores = l.assets.as_ref().and_then(|a| a.logical_cores);
            if l.online {
                since.remove(&*l.agent_id);
                entry.offline_at = None;
            } else {
                // 離線的時間只算一次（之後每個 tick 相同，事件的「異常的開始」才穩定）
                let at = *since.entry(l.agent_id.to_string()).or_insert_with(|| {
                    match (l.seen_since_start, l.offline_for) {
                        (true, Some(d)) => now - d.as_millis() as i64,
                        _ => l.last_seen.timestamp_millis(),
                    }
                });
                entry.offline_at = Some(at);
            }
        }
        since.retain(|k, _| live_ids.contains(k));
        out.into_values().collect()
    }

    fn rejections(&self) -> Vec<Rejection> {
        let revoked = self
            .revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        crate::tls::rejections()
            .all()
            .into_iter()
            .map(|(agent_id, r)| Rejection {
                agent_id,
                at_ms: r.at.timestamp_millis(),
                reason: r.reason.as_str().to_string(),
                code: r.reason.product_code(),
                admin_revoked: r.reason == crate::tls::RejectionReason::Revoked
                    && r.serial.as_ref().is_some_and(|s| revoked.contains(s)),
                serial: r.serial,
                ip: r.peer,
            })
            .collect()
    }

    /// 一個判定週期
    async fn tick(&self) {
        let started = std::time::Instant::now();
        let now = (self.clock)();
        let hosts = self.world_hosts(now);
        let rejections = self.rejections();
        let facts = self
            .db_facts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let channels = self.dispatcher.channels();
        self.watch_secrets(&channels);
        let mut engine = self.engine.lock().await;
        let (conditions, checked) = {
            let mut probe = self.probe.lock().unwrap_or_else(|e| e.into_inner());
            probe.check(
                chrono::DateTime::from_timestamp_millis(now).unwrap_or_else(Utc::now),
                engine.rules(),
                facts.as_ref(),
            )
        };
        engine.set_channels(channels);
        engine.tick(World {
            now_ms: now,
            hosts,
            conditions,
            rejections,
            checked,
        });
        let out = engine.take_output();
        drop(engine);
        self.last_tick.store(now, Ordering::Relaxed);
        self.flush(out).await;
        self.tick_micros
            .store(started.elapsed().as_micros() as i64, Ordering::Relaxed);
    }

    /// 設定（規則、靜音、管道）有變更時重新載入
    async fn maybe_reload(&self) {
        let forced = self.reload.swap(false, Ordering::SeqCst);
        let rev = match self.db.read(|conn| Ok(store::config_rev(conn)?)).await {
            Ok(r) => r,
            Err(e) => {
                self.note_error(format!("無法讀取告警設定的版本：{e}"));
                return;
            }
        };
        if !forced && rev == self.config_rev.load(Ordering::SeqCst) {
            return;
        }
        let now = (self.clock)();
        let loaded = self
            .db
            .read(move |conn| {
                Ok((
                    store::load_rules(conn)?,
                    store::load_silences(conn, now - 86_400_000)?,
                ))
            })
            .await;
        match loaded {
            Ok((rules, silences)) => {
                self.config_rev.store(rev, Ordering::SeqCst);
                let mut engine = self.engine.lock().await;
                engine.set_rules(rules, now);
                engine.set_silences(silences);
                let out = engine.take_output();
                drop(engine);
                self.dispatcher.reload_channels();
                self.flush(out).await;
            }
            Err(e) => self.note_error(format!("無法重新載入告警設定：{e}")),
        }
    }

    /// 要求下一輪重新載入設定（API 修改規則、靜音、管道之後）
    pub fn request_reload(&self) {
        self.reload.store(true, Ordering::SeqCst);
        self.dispatcher.reload_channels();
    }

    /// 寫入引擎的輸出、排入通知、推送 SSE；寫入失敗時保留到下一次
    pub async fn flush(&self, out: Output) {
        let mut out = {
            let mut retry = self.retry.lock().unwrap_or_else(|e| e.into_inner());
            if retry.is_empty() {
                out
            } else {
                let mut merged = std::mem::take(&mut *retry);
                let ids: HashSet<String> = out.events.iter().map(|e| e.event_id.clone()).collect();
                merged.events.retain(|e| !ids.contains(&e.event_id));
                merged.events.extend(out.events);
                merged.timeline.extend(out.timeline);
                merged.deliveries.extend(out.deliveries);
                merged.changes.extend(out.changes);
                merged.silences_notified.extend(out.silences_notified);
                merged
            }
        };
        if out.is_empty() {
            return;
        }
        let changes = std::mem::take(&mut out.changes);
        for c in changes {
            let _ = self.changes.send(c);
        }
        // 大量事件（例如第一個週期一次開出數千個）分成多個短交易寫入，不長時間佔用寫入鎖
        while !out.events.is_empty() || !out.timeline.is_empty() {
            let ev: Vec<_> = out
                .events
                .drain(..out.events.len().min(FLUSH_CHUNK))
                .collect();
            let tl: Vec<_> = out
                .timeline
                .drain(..out.timeline.len().min(FLUSH_CHUNK * 2))
                .collect();
            let (ev2, tl2) = (ev.clone(), tl.clone());
            let r = self
                .db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    store::upsert_events(&tx, &ev2)?;
                    store::insert_timeline(&tx, &tl2)?;
                    tx.commit()?;
                    Ok(())
                })
                .await;
            if let Err(e) = r {
                self.note_error(format!("無法寫入告警事件（稍後重試）：{e}"));
                let mut rest = ev;
                rest.append(&mut out.events);
                out.events = rest;
                let mut rest = tl;
                rest.append(&mut out.timeline);
                out.timeline = rest;
                *self.retry.lock().unwrap_or_else(|x| x.into_inner()) = out;
                return;
            }
        }
        let deliveries = out.deliveries.clone();
        let silences = out.silences_notified.clone();
        let result = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let ids = store::insert_deliveries(&tx, &deliveries)?;
                store::mark_silences_notified(&tx, &silences)?;
                tx.commit()?;
                Ok(ids)
            })
            .await;
        match result {
            Ok(ids) => {
                let items = out
                    .deliveries
                    .iter()
                    .zip(ids)
                    .map(|(d, id)| {
                        let mut payload = d.payload.clone();
                        if let (Value::Object(m), Some(g)) = (&mut payload, &d.group) {
                            m.insert("group".into(), json!(g));
                        }
                        store::Delivery {
                            id,
                            event_id: d.event_id.clone(),
                            channel_id: d.channel_id.clone(),
                            kind: d.kind.to_string(),
                            state: "pending".into(),
                            urgent: d.urgent,
                            payload,
                            attempts: 0,
                            created_at: d.created_at,
                            next_attempt_at: d.created_at,
                            sent_at: None,
                            batch_id: None,
                            response_code: None,
                            error_code: None,
                            error: None,
                        }
                    })
                    .collect();
                self.dispatcher.enqueue(items);
            }
            Err(e) => {
                self.note_error(format!("無法寫入告警事件（稍後重試）：{e}"));
                *self.retry.lock().unwrap_or_else(|x| x.into_inner()) = out;
            }
        }
    }

    /// CLI 交給執行中的 Kairos 的動作
    async fn process_actions(&self) {
        let actions = match self.db.read(|conn| Ok(store::pending_actions(conn)?)).await {
            Ok(a) if !a.is_empty() => a,
            _ => return,
        };
        let now = (self.clock)();
        let mut results = Vec::new();
        {
            let mut engine = self.engine.lock().await;
            for a in actions {
                let actor = format!("cli:{}", a.actor);
                let r = apply_action(&mut engine, &a.event_id, &a.action, &a.params, &actor, now);
                let result = match r {
                    Ok(v) => json!({ "ok": true, "data": v }),
                    Err(e) => json!({ "ok": false, "code": e.code, "message": e.message }),
                };
                results.push((a, result));
            }
            let out = engine.take_output();
            drop(engine);
            self.flush(out).await;
        }
        if let Err(e) = self
            .db
            .write(move |conn| {
                for (a, r) in &results {
                    store::finish_action(conn, a.id, now, r)?;
                    // 稽核雜湊鏈：與主控台 API 的確認、解決、註解相同的紀錄（執行者為 CLI 的作業系統帳號）
                    let ok = r["ok"] == true;
                    crate::enroll::store::audit(
                        conn,
                        now,
                        &crate::enroll::store::Audit {
                            actor: &a.actor,
                            actor_ip: None,
                            category: "alerts",
                            action: &format!("alert.{}", a.action),
                            object: Some(&a.event_id),
                            result: if ok { "ok" } else { "failed" },
                            code: None,
                            details: Some(json!({ "params": a.params, "via": "cli" })),
                        },
                    )?;
                }
                Ok(())
            })
            .await
        {
            self.note_error(format!("無法寫入 CLI 動作的結果：{e}"));
        }
    }

    async fn purge(&self) {
        let now = (self.clock)();
        let days = self.settings.retention_days;
        let max = self.settings.max_events;
        loop {
            let r = self
                .db
                .write(move |conn| Ok(store::purge(conn, now, days, max, 2000)?))
                .await;
            match r {
                Ok((p, more)) => {
                    if p.events > 0 {
                        log::info!(
                            event = "alerts.retention.purged";
                            "已依保留期限與筆數上限刪除告警歷史：事件 {} 個、處理紀錄 {} 筆、發送紀錄 {} 筆（alerts.retention_days = {days}、alerts.max_events = {max}）",
                            p.events,
                            p.timeline,
                            p.deliveries
                        );
                    }
                    if !more {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => {
                    self.note_error(format!("無法清除告警歷史：{e}"));
                    break;
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // API 用
    // ------------------------------------------------------------------

    /// SSE 的變化
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    /// 對一個事件執行使用者的動作（API 與 CLI 共用）；回傳結果與更新後的事件
    pub async fn act(
        &self,
        event_id: &str,
        action: &str,
        params: &Value,
        actor: &str,
    ) -> Result<Value, ActionError> {
        let now = (self.clock)();
        let mut engine = self.engine.lock().await;
        let r = apply_action(&mut engine, event_id, action, params, actor, now);
        let out = engine.take_output();
        drop(engine);
        self.flush(out).await;
        r
    }

    /// 規則的快照
    pub async fn rules(&self) -> Vec<model::Rule> {
        self.engine.lock().await.rules().to_vec()
    }

    /// 範圍內（使用中的）主機數（S10「目前符合 N 台主機」）
    pub async fn matching_hosts(&self, scope: &Scope) -> usize {
        let hosts = self.engine.lock().await.hosts();
        let c = scope::Compiled::new(scope);
        hosts
            .iter()
            .filter(|h| !h.decommissioned && c.matches(&h.facts()))
            .count()
    }

    /// 以主機為範圍的靜音涵蓋的主機（S06「目前有 2 個靜音規則生效，影響 301 台主機」）
    pub async fn matching_hosts_list(&self, silences: &[model::Silence]) -> Vec<String> {
        let hosts = self.engine.lock().await.hosts();
        let compiled: Vec<scope::Compiled> = silences
            .iter()
            .map(|s| scope::Compiled::new(&s.scope))
            .collect();
        hosts
            .iter()
            .filter(|h| !h.decommissioned && compiled.iter().any(|c| c.matches(&h.facts())))
            .map(|h| h.agent_id.to_string())
            .collect()
    }

    /// 引擎的狀態（S06「告警引擎未在執行」）
    pub async fn status(&self) -> Value {
        let now = (self.clock)();
        let last = self.last_tick.load(Ordering::Relaxed);
        let engine = self.engine.lock().await;
        let w = engine.warmup();
        let running = !self.stopped.load(Ordering::SeqCst) && last > 0 && now - last < 60_000;
        json!({
            "running": running,
            "started_at": crate::api::http::ts_ms(self.started_ms),
            "last_tick_at": (last > 0).then(|| crate::api::http::ts_ms(last)),
            "last_tick_duration_ms": self.tick_micros.load(Ordering::Relaxed) as f64 / 1000.0,
            "warmup": {
                "active": !w.ended,
                "until": crate::api::http::ts_ms(w.until_ms),
                "expected_hosts": w.expected.len(),
                "missing_hosts": w.missing.len(),
            },
            "bus_dropped": self.registry.bus().dropped(),
            "last_error": self.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        })
    }

    /// `/metrics` 用的狀態（[`MetricsSnapshot`]；標籤只有嚴重度、事件狀態、管道狀態的有限集合）
    pub async fn metrics(&self) -> MetricsSnapshot {
        let now = (self.clock)();
        let last = self.last_tick.load(Ordering::Relaxed);
        let mut open = [[0u64; 3]; 4];
        let warmup;
        {
            let engine = self.engine.lock().await;
            warmup = !engine.warmup().ended;
            for e in engine.events.values() {
                let st = match e.state {
                    model::EventState::Firing => 0,
                    model::EventState::Acknowledged => 1,
                    model::EventState::Recovered => 2,
                    model::EventState::Resolved => continue,
                };
                let sev = model::Severity::ALL
                    .iter()
                    .position(|s| *s == e.severity)
                    .unwrap_or(3);
                open[sev][st] += 1;
            }
        }
        let mut channels = [0u64; 3];
        for c in self.dispatcher.channels() {
            let i = if !c.enabled {
                2
            } else if c.failed() {
                1
            } else {
                0
            };
            channels[i] += 1;
        }
        MetricsSnapshot {
            running: !self.stopped.load(Ordering::SeqCst) && last > 0 && now - last < 60_000,
            last_tick_ms: last,
            tick_seconds: self.tick_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            warmup,
            open,
            channels,
        }
    }

    /// 機密無法解開的管道：狀態轉換時記一次 WARN（`KRS-E5110`）並發布內部事件
    /// （[`crate::bus::EventKind::ChannelSecrets`]）；事件本身由「通知管道失敗」系統規則開立
    fn watch_secrets(&self, channels: &[model::Channel]) {
        let now: HashSet<String> = channels
            .iter()
            .filter(|c| c.secrets_unavailable())
            .map(|c| c.channel_id.clone())
            .collect();
        let mut prev = self
            .secrets_missing
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *prev == now {
            return;
        }
        for c in channels {
            let id = &c.channel_id;
            match (prev.contains(id), now.contains(id)) {
                (false, true) => {
                    log::warn!(
                        event = "alerts.channel.secrets_unavailable", code = "KRS-E5110";
                        "錯誤 KRS-E5110：通知管道「{}」（{id}）的機密無法解開：資料目錄的 alerts-secrets.key 遺失或與資料庫不成對。這個管道不會發送通知；請從備份還原金鑰檔（kairos db restore-keys）後重新啟動，或在主控台重新輸入機密",
                        c.name.escape_debug()
                    );
                    self.registry
                        .bus()
                        .publish(crate::bus::EventKind::ChannelSecrets {
                            channel_id: id.clone(),
                            name: c.name.clone(),
                            available: false,
                        });
                }
                (true, false) => {
                    log::info!(
                        event = "alerts.channel.secrets_restored";
                        "通知管道「{}」（{id}）的機密已可解開",
                        c.name.escape_debug()
                    );
                    self.registry
                        .bus()
                        .publish(crate::bus::EventKind::ChannelSecrets {
                            channel_id: id.clone(),
                            name: c.name.clone(),
                            available: true,
                        });
                }
                _ => {}
            }
        }
        *prev = now;
    }

    /// 「寄送測試通知」的限速；超過時回傳限制的內容
    pub fn check_test_limit(
        &self,
        channel: Option<&str>,
        actor: &str,
        now: i64,
    ) -> Result<(), test_limit::Limited> {
        self.test_limits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(channel, actor, now)
    }

    /// 派送端的管道狀態（API 的「最近發送」與狀態欄）
    pub fn channel_status(&self, id: &str) -> Option<Channel> {
        self.dispatcher
            .channels()
            .into_iter()
            .find(|c| c.channel_id == id)
    }

    pub fn dispatch_config(&self) -> DispatchConfig {
        DispatchConfig {
            offset: self.settings.offset(),
            console_url: self.settings.console_url.clone(),
        }
    }

    /// 等候引擎的執行緒結束（關機時；最多 `limit`）
    pub async fn join(thread: std::thread::JoinHandle<()>, limit: Duration) {
        let waited = tokio::task::spawn_blocking(move || {
            let _ = thread.join();
        });
        let _ = tokio::time::timeout(limit, waited).await;
    }
}

/// 使用者動作的分派（`ack`、`assign`、`snooze`、`resolve`、`comment`）
pub fn apply_action(
    engine: &mut Engine,
    event_id: &str,
    action: &str,
    params: &Value,
    actor: &str,
    now: i64,
) -> Result<Value, ActionError> {
    let bad = |m: &str| ActionError {
        code: "invalid_parameter",
        message: m.to_string(),
    };
    match action {
        "ack" => {
            let assign = params
                .get("assign_to_me")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let eta = match params.get("eta_minutes") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .filter(|m| (1..=1440).contains(m))
                        .ok_or_else(|| bad("eta_minutes 必須是 1～1440"))?
                        as u32,
                ),
            };
            let changed = engine.ack(event_id, actor, assign, eta, now)?;
            Ok(json!({ "acknowledged": true, "already": !changed }))
        }
        "assign" => {
            let who = params
                .get("assignee")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("assignee 必填"))?;
            engine.assign(event_id, actor, who, now)?;
            Ok(json!({ "assignee": who }))
        }
        "snooze" => {
            let hours = params
                .get("hours")
                .and_then(Value::as_u64)
                .ok_or_else(|| bad("hours 必填（1、4 或 8）"))?;
            engine.snooze(event_id, actor, hours.min(1000) as u32, now)?;
            Ok(json!({ "snoozed_hours": hours }))
        }
        "resolve" => {
            let cause = params
                .get("root_cause")
                .and_then(Value::as_str)
                .and_then(RootCause::parse_user)
                .ok_or_else(|| bad("root_cause 必填：hardware、network、unregistered_maintenance、software、false_positive、other"))?;
            let note = params
                .get("note")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string);
            if note.as_ref().is_some_and(|n| n.chars().count() > 2000) {
                return Err(bad("note 最多 2000 個字元"));
            }
            let detached = engine.resolve(event_id, actor, cause, note, now)?;
            Ok(json!({ "resolved": true, "detached_children": detached }))
        }
        "comment" => {
            let text = params
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("text 必填"))?;
            engine.comment(event_id, actor, text, now)?;
            Ok(json!({ "commented": true }))
        }
        _ => Err(bad("不支援的動作")),
    }
}
