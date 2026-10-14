//! 註冊通道（8444，`crates/kairos-proto/ENROLL.md`）與線上 PKI 的常駐服務。
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`store`] | 註冊相關資料表的讀寫 |
//! | [`ops`] | 管理操作（token、核准、拒絕、撤銷、模式）：主控台 API 與 CLI 共用 |
//! | [`limits`] | L1～L11 的速率限制 |
//! | [`db`] | 註冊服務的資料庫連線 |
//! | [`pki_mgr`] | 簽發器、CRL、信任錨的檔案 |
//! | [`http`] | 8444 的回應格式 |
//! | [`handlers`] | 8444 的端點 |
//! | [`server`] | 8444 的 listener（TLS、HTTP/1.1、L1～L3、模式切換時的監聽與停止） |
//! | [`agent`] | 8443 上的 `renew`、`deregister` 與輪替後撤銷舊憑證 |
//!
//! 服務在自己的 tokio runtime（2 個工作執行緒）上執行，與 agent 連線、report 的 writer 分開排程：
//! 8444 的 TLS 握手與 JSON 處理不會排在 8443 的 `ack` 前面（量測見 `kairos/PKI.md` 第 12 節）。

pub mod agent;
pub mod db;
pub mod handlers;
pub mod http;
pub mod limits;
pub mod ops;
pub mod pki_mgr;
pub mod server;
pub mod store;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;

use kairos_proto::enroll::EnrollMode;
use log::{debug, error, info, warn};
use rusqlite::Connection;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use crate::config::{AppConfig, PkiMode};
use crate::per_ip::Allowlist;
use crate::settings::{self, Effective};
use crate::shutdown::Shutdown;
use db::{DbError, EnrollDb};
use limits::{Block, Gcra, InvalidClass, InvalidPolicy, InvalidTracker, Keyed, Slots};
use pki_mgr::{IssueStatus, PkiManager};
use store::{Audit, BlockRow};

/// 背景工作的週期：設定（主控台／CLI 寫入的模式）、CLI 核准的申請、撤銷與 CRL、外部變更的長輪詢通知
pub const TICK: Duration = Duration::from_secs(2);
/// 撤回 nonce 的上限筆數（記憶體）
pub const MAX_NONCES: usize = 100_000;
/// 結束的申請保存 30 天（S13「最近結束」）
pub const CLOSED_KEEP_MS: i64 = 30 * 86_400_000;
/// 除役後只受理 deregister 的視窗（ENROLL.md 第 18 節第 1 項）
pub const DEREGISTER_WINDOW_MS: i64 = 10 * 60_000;
/// 時鐘被拒的主機在 token 的部署進度中保留 15 分鐘（API.md 第 11.2 節）
pub const CLOCK_REJECTED_KEEP_MS: i64 = 15 * 60_000;

/// 下載端點（`/agent/<檔名>`）本版回 404（ENROLL.md 第 19.4 節第 3 項）；主控台據此隱藏一行指令與下載連結
/// （設計包 INTEGRATION §27.4 #3）
pub const DOWNLOADS_AVAILABLE: bool = false;

/// 下載端點的狀態（主控台 API 的 `downloads`）
pub fn downloads_json() -> serde_json::Value {
    json!({
        "available": DOWNLOADS_AVAILABLE,
        "reason": "本版的註冊端點不提供安裝腳本與套件的下載（/agent/ 回 404）",
        "alternative": "以安裝套件（MSI、deb、rpm）安裝 Pulse 後執行 pulse enroll --install-string <安裝字串>",
    })
}

/// 現在時間（Unix 毫秒）；測試可替換
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| chrono::Utc::now().timestamp_millis())
}

/// 註冊端點的實際狀態（`GET /api/v1/enroll/mode` 的 `state`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeState {
    Open,
    OpenPaused,
    RenewOnly,
    Closed,
}

impl ModeState {
    pub fn as_str(self) -> &'static str {
        match self {
            ModeState::Open => "open",
            ModeState::OpenPaused => "open_paused",
            ModeState::RenewOnly => "renew_only",
            ModeState::Closed => "closed",
        }
    }
}

/// 8444 是否在監聽與原因
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListenState {
    pub listening: bool,
    pub addr: Option<String>,
    /// 沒有監聽的原因（模式為關閉、缺少 `pki.server_ca_path`、連接埠被佔用等）
    pub reason: Option<String>,
}

/// S20 與系統健康的計數（啟動以來）
#[derive(Debug, Default)]
pub struct EnrollStats {
    pub requests: AtomicU64,
    pub enroll_requests: AtomicU64,
    pub renew_requests: AtomicU64,
    pub limited_enroll: AtomicU64,
    pub limited_renew: AtomicU64,
    pub blocked: AtomicU64,
    pub busy: AtomicU64,
    pub clock_rejected: AtomicU64,
    pub pre_handshake_closed: AtomicU64,
    pub issued: AtomicU64,
    pub pending_created: AtomicU64,
    /// 自上次彙總記錄以來時鐘被拒的次數（`KRS-W8302` 每分鐘彙總一行）
    pub clock_since_log: AtomicU64,
}

/// 長輪詢的登記：(通知, 等待中的數量, 登記時的狀態)
type Waiter = (Arc<Notify>, usize, String);

/// 註冊服務
pub struct EnrollService {
    pub config: Arc<AppConfig>,
    pub data_dir: PathBuf,
    pub db: Arc<EnrollDb>,
    pub pki: PkiManager,
    clock: Clock,
    effective: RwLock<Effective>,
    /// 開放模式下新註冊已自動暫停
    paused: AtomicBool,
    paused_since: Mutex<Option<i64>>,
    pub limits: Limits,
    /// request_id → (nonce, 到期時間)
    nonces: Mutex<HashMap<String, (String, i64)>>,
    /// 長輪詢中的申請 → 通知
    waiters: Mutex<HashMap<String, Waiter>>,
    pub listen: Mutex<ListenState>,
    /// 簽章內容最後一行可接受的註冊端點名稱（ENROLL.md 第 16.3 節）
    names: RwLock<Vec<String>>,
    /// 批次部署來源（設定＋暫時放寬＋批次部署模式的 token 網段）
    batch: RwLock<Allowlist>,
    pub stats: EnrollStats,
    /// 已除役的主機（8443 的 hello 判定；啟動時由清冊載入）
    decommissioned: RwLock<HashSet<String>>,
    /// 已記錄第一次使用的序號（避免每次 hello 都寫資料庫）
    first_used: Mutex<HashSet<String>>,
    /// agent_id → (token_id, 時間)：最近被時鐘檢查拒絕（部署進度的 `clock_rejected`）
    pub clock_rejected: Mutex<HashMap<String, (Option<String>, i64)>>,
    /// agent_id → (token_id, 時間)：以這個 token 送出、目前被 429 延後（部署進度的 `queued`）
    pub queued: Mutex<HashMap<String, (String, i64)>>,
    /// 喚醒背景工作（主控台修改模式後立即生效）
    wake: Notify,
    pub shutdown: Arc<Shutdown>,
    started_ms: i64,
    last_hourly: Mutex<i64>,
    last_minutely: Mutex<i64>,
    last_crl_refresh: Mutex<HashMap<i64, i64>>,
    /// 上次記錄 `KRS-W1304` 的時間（每 24 小時最多一次）
    last_expiry_warning: Mutex<i64>,
}

/// 各層的限流狀態
pub struct Limits {
    /// L1：全域新連線
    pub conn_global: Mutex<Gcra>,
    /// L2：每個位址的同時連線
    pub conn_per_addr: Arc<Slots<IpAddr>>,
    /// L3：每個來源的新連線（軟上限、硬上限＝4 倍）
    pub conn_soft: Keyed<IpAddr>,
    pub conn_hard: Keyed<IpAddr>,
    /// L4／L4r：每個來源
    pub l4: Keyed<IpAddr>,
    pub l4r: Keyed<IpAddr>,
    /// L5／L5b：每個 token
    pub l5: Keyed<String>,
    pub l5b: Keyed<String>,
    /// L6／L6r：每個 agent_id
    pub l6: Keyed<String>,
    pub l6r: Keyed<String>,
    /// L7
    pub l7: InvalidTracker,
    /// L8：全域申請速率
    pub l8: Mutex<Gcra>,
    /// L10：每筆申請
    pub l10: Keyed<String>,
    /// L10：長輪詢名額
    pub long_polls: Arc<Slots<IpAddr>>,
    /// 8443 的 renew：每個 agent_id 每小時 10 次（ENROLL.md 第 13.1 節）
    pub renew_8443: Keyed<String>,
}

impl Limits {
    fn new(c: &crate::config::EnrollSettings) -> Self {
        let now = Instant::now();
        Self {
            conn_global: Mutex::new(Gcra::new(
                f64::from(c.max_new_connections_burst),
                f64::from(c.max_new_connections_per_sec),
                now,
            )),
            conn_per_addr: Arc::new(Slots::default()),
            conn_soft: Keyed::new(),
            conn_hard: Keyed::new(),
            l4: Keyed::new(),
            l4r: Keyed::new(),
            l5: Keyed::new(),
            l5b: Keyed::new(),
            l6: Keyed::new(),
            l6r: Keyed::new(),
            l7: InvalidTracker::new(invalid_policy(c)),
            l8: Mutex::new(Gcra::new(
                f64::from(c.max_requests_burst),
                f64::from(c.max_requests_per_sec),
                now,
            )),
            l10: Keyed::new(),
            long_polls: Arc::new(Slots::default()),
            renew_8443: Keyed::new(),
        }
    }
}

fn invalid_policy(c: &crate::config::EnrollSettings) -> InvalidPolicy {
    InvalidPolicy {
        enroll_limit: c.max_invalid_tokens_per_ip as usize,
        renew_limit: c.max_invalid_renewals_per_ip as usize,
        window_ms: i64::from(c.invalid_window_secs) * 1000,
        block_initial_ms: i64::from(c.block_initial_secs) * 1000,
        block_max_ms: i64::from(c.block_max_secs) * 1000,
    }
}

/// [`EnrollService::start`] 的參數
pub struct StartParams {
    pub config: Arc<AppConfig>,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub shutdown: Arc<Shutdown>,
    /// 8443、主控台的監聽位址（`KRS-E0202` 的檢查）
    pub other_listens: Vec<std::net::SocketAddr>,
    pub clock: Clock,
}

impl EnrollService {
    /// 建立服務：載入設定、模式、簽發 CA、封鎖表、除役清單（同步讀資料庫；在 blocking 執行緒呼叫）
    pub fn new(p: &StartParams) -> Result<Arc<Self>, String> {
        let db = EnrollDb::new(p.db_path.clone());
        let pki = PkiManager::new(&p.data_dir, p.config.pki.effective_mode());
        if let Some(path) = p.config.pki.server_ca() {
            match pki_mgr::load_server_ca(path) {
                Ok(ca) => {
                    *pki.server_ca.write().unwrap_or_else(|e| e.into_inner()) = Some(ca);
                }
                Err(e) => warn!("pki.server_ca_path 無法載入，註冊端點不監聽：{e}"),
            }
        }
        let now = (p.clock)();
        let (overrides, blocks, decommissioned, rows) = db
            .write_blocking(|conn| {
                let rows = store::load_settings(conn)?;
                let blocks = store::load_blocks(conn)?;
                let mut stmt =
                    conn.prepare("SELECT agent_id FROM hosts WHERE lifecycle = 'decommissioned'")?;
                let decom: HashSet<String> = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok((settings::overrides_from(&rows), blocks, decom, rows))
            })
            .map_err(|e| format!("無法讀取註冊設定：{e}"))?;
        let eff = settings::effective(&p.config, &overrides);
        let overridden = settings::overridden_file_settings(&p.config, &rows);
        if !overridden.is_empty() {
            info!(
                "主控台修改的設定優先於 kairos.toml（存在資料庫）：{}；以 kairos config show --effective 查看，kairos config reset <設定鍵> 改用設定檔的值",
                overridden.join("、")
            );
        }
        let limits = Limits::new(&p.config.enroll);
        for b in blocks {
            if let (Ok(ip), Some(class)) =
                (b.source.parse::<IpAddr>(), InvalidClass::parse(&b.class))
            {
                limits.l7.restore(
                    ip,
                    class,
                    Block {
                        until_ms: b.blocked_until,
                        offenses: b.offenses,
                        last_offense_ms: b.last_offense_at,
                    },
                );
            }
        }
        let batch = p.config.enroll.batch_allowlist().unwrap_or_default();
        let svc = Arc::new(Self {
            config: p.config.clone(),
            data_dir: p.data_dir.clone(),
            db,
            pki,
            clock: p.clock.clone(),
            effective: RwLock::new(eff),
            paused: AtomicBool::new(false),
            paused_since: Mutex::new(None),
            limits,
            nonces: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            listen: Mutex::new(ListenState::default()),
            names: RwLock::new(Vec::new()),
            batch: RwLock::new(batch),
            stats: EnrollStats::default(),
            decommissioned: RwLock::new(decommissioned),
            first_used: Mutex::new(HashSet::new()),
            clock_rejected: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            shutdown: p.shutdown.clone(),
            started_ms: now,
            last_hourly: Mutex::new(0),
            last_minutely: Mutex::new(0),
            last_crl_refresh: Mutex::new(HashMap::new()),
            last_expiry_warning: Mutex::new(0),
        });
        // 簽發 CA、信任錨與 CRL：啟動時同步一次（8443 的 TLS 驗證器需要它們）
        svc.sync_pki(now, true);
        svc.evaluate_pause(now);
        Ok(svc)
    }

    pub fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    pub fn effective(&self) -> Effective {
        self.effective
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 開放模式下新註冊是否已自動暫停
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn paused_since(&self) -> Option<i64> {
        *self.paused_since.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 實際狀態（開放／開放·自動暫停／僅續簽／關閉）
    pub fn mode_state(&self) -> ModeState {
        match self.effective().mode {
            EnrollMode::Open if self.is_paused() => ModeState::OpenPaused,
            EnrollMode::Open => ModeState::Open,
            EnrollMode::RenewOnly => ModeState::RenewOnly,
            EnrollMode::Closed => ModeState::Closed,
        }
    }

    /// 8444 監聽所需的條件（不含模式）；不滿足時回傳原因
    pub fn prerequisites(&self) -> Result<(), String> {
        if self.config.auth.client_ca_path().is_none() {
            return Err("未啟用 mTLS（auth.client_ca）：簽發的憑證無法用於 8443".into());
        }
        if self.config.pki.server_ca().is_none() {
            return Err("未設定 pki.server_ca_path：沒有被釘選的 CA，Pulse 無法驗證 Kairos".into());
        }
        if self.pki.server_ca().is_none() {
            return Err("pki.server_ca_path 無法載入".into());
        }
        Ok(())
    }

    /// 簽章內容可接受的註冊端點名稱
    pub fn endpoint_names(&self) -> Vec<String> {
        self.names.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_endpoint_names(&self, names: Vec<String>) {
        *self.names.write().unwrap_or_else(|e| e.into_inner()) = names;
    }

    /// 來源的倍數（批次部署來源為 `enroll.batch_source_multiplier`，其他為 1）
    pub fn multiplier(&self, ip: IpAddr) -> f64 {
        if self
            .batch
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(ip)
        {
            f64::from(self.config.enroll.batch_source_multiplier)
        } else {
            1.0
        }
    }

    pub fn is_batch_source(&self, ip: IpAddr) -> bool {
        self.multiplier(ip) > 1.0
    }

    /// 喚醒背景工作（模式改變、核准、撤銷後）
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// 8443 的 features 中與註冊有關的部分（`renew`、`deregister`）
    pub fn agent_features(&self) -> Vec<&'static str> {
        kairos_proto::LIFECYCLE_FEATURES.to_vec()
    }

    // ---- 長輪詢 ----

    /// 登記長輪詢；回傳通知與登記的鍵（取消時以 [`EnrollService::unwatch`] 移除）
    pub fn watch(&self, request_id: &str, state: &str) -> Arc<Notify> {
        let mut w = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        let entry = w
            .entry(request_id.to_string())
            .or_insert_with(|| (Arc::new(Notify::new()), 0, state.to_string()));
        entry.1 += 1;
        entry.2 = state.to_string();
        entry.0.clone()
    }

    pub fn unwatch(&self, request_id: &str) {
        let mut w = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = w.get_mut(request_id) {
            entry.1 = entry.1.saturating_sub(1);
            if entry.1 == 0 {
                w.remove(request_id);
            }
        }
    }

    /// 申請的狀態改變：喚醒長輪詢
    pub fn notify_request(&self, request_id: &str) {
        let w = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((n, ..)) = w.get(request_id) {
            n.notify_waiters();
        }
    }

    // ---- 撤回的 nonce ----

    /// 發出新的 nonce（作廢同一筆申請舊的 nonce）
    pub fn issue_nonce(&self, request_id: &str, nonce: String, expires_ms: i64) {
        let mut n = self.nonces.lock().unwrap_or_else(|e| e.into_inner());
        if n.len() >= MAX_NONCES && !n.contains_key(request_id) {
            let now = self.now_ms();
            n.retain(|_, (_, exp)| *exp > now);
        }
        n.insert(request_id.to_string(), (nonce, expires_ms));
    }

    /// 取出（並作廢）這筆申請的 nonce：任何一次驗證嘗試都作廢
    pub fn take_nonce(&self, request_id: &str) -> Option<(String, i64)> {
        self.nonces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(request_id)
    }

    // ---- 除役 ----

    pub fn is_decommissioned(&self, agent_id: &str) -> bool {
        self.decommissioned
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(agent_id)
    }

    pub fn set_decommissioned(&self, agent_id: &str, yes: bool) {
        let mut d = self
            .decommissioned
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if yes {
            d.insert(agent_id.to_string());
        } else {
            d.remove(agent_id);
        }
    }

    /// 第一次以這個序號 hello（在記憶體中記錄；回傳是否為本次啟動以來第一次）
    pub fn first_seen_serial(&self, serial: &str) -> bool {
        self.first_used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(serial.to_string())
    }

    // ---- L7 ----

    /// 記錄無效值；開始封鎖時寫入資料庫並記 `KRS-W3005`
    pub async fn record_invalid(
        self: &Arc<Self>,
        source: IpAddr,
        class: InvalidClass,
        value: [u8; 32],
    ) {
        let now = self.now_ms();
        if let Some(b) = self.limits.l7.record(source, class, value, now) {
            warn!(
                "警告 KRS-W3005：已暫時封鎖 {} 的{}請求至 {}：{} 內嘗試了 {} 個以上不同的無效{}（第 {} 次封鎖）",
                source,
                match class {
                    InvalidClass::Enroll => "新註冊類",
                    InvalidClass::Renew => "過期續簽類",
                },
                crate::api::http::ts_ms(b.until_ms),
                humanize_ms(self.config.enroll.invalid_window_secs as i64 * 1000),
                match class {
                    InvalidClass::Enroll => self.config.enroll.max_invalid_tokens_per_ip,
                    InvalidClass::Renew => self.config.enroll.max_invalid_renewals_per_ip,
                },
                match class {
                    InvalidClass::Enroll => "token 或輪詢憑據",
                    InvalidClass::Renew => "簽章或主機指紋",
                },
                b.offenses + 1
            );
            let row = BlockRow {
                source: source.to_string(),
                class: class.as_str().to_string(),
                blocked_until: b.until_ms,
                offenses: b.offenses,
                last_offense_at: b.last_offense_ms,
            };
            let audit_source = source.to_string();
            let result = self
                .db
                .write(move |conn| {
                    store::upsert_block(conn, &row)?;
                    store::audit(
                        conn,
                        now,
                        &Audit {
                            actor: "system",
                            actor_ip: Some(&audit_source),
                            category: "enroll",
                            action: "source.block",
                            object: Some(&audit_source),
                            result: "ok",
                            code: Some("KRS-W3005"),
                            details: Some(json!({"class": row.class, "until": crate::api::http::ts_ms(row.blocked_until), "offenses": row.offenses})),
                        },
                    )?;
                    Ok(())
                })
                .await;
            if let Err(e) = result {
                warn!("無法把 {source} 的封鎖寫入資料庫（重新啟動後不保留）：{e}");
            }
        }
    }

    // ---- 背景工作 ----

    /// 重新讀取 `settings` 表；模式或政策改變時記錄
    fn refresh_settings(&self, conn: &Connection) -> Result<(), DbError> {
        let rows = store::load_settings(conn)?;
        let eff = settings::effective(&self.config, &settings::overrides_from(&rows));
        let mut cur = self.effective.write().unwrap_or_else(|e| e.into_inner());
        if cur.mode != eff.mode {
            info!(
                "註冊端點模式：{} → {}（來源 {}）",
                cur.mode.as_str(),
                eff.mode.as_str(),
                eff.source(settings::KEY_MODE).as_str()
            );
        }
        if cur.policy != eff.policy {
            info!(
                "憑證政策已更新：有效 {} 天、到期前 {} 天輪替、輪替需核准 {}、過期寬限期 {} 天",
                eff.policy.cert_lifetime_days,
                eff.policy.renew_before_days,
                eff.policy.renew_requires_approval,
                eff.policy.expired_renew_grace_days
            );
        }
        *cur = eff;
        Ok(())
    }

    /// 自動暫停（ENROLL.md 第 10 節）：開放模式、啟用自動暫停，連續 N 小時沒有有效 token 時暫停新註冊；
    /// 建立 token 時解除。暫停與解除都寫稽核
    fn evaluate_pause(&self, now: i64) {
        let eff = self.effective();
        let computed = self.db.read_blocking(|conn| {
            let (active, last_end) = store::token_validity(conn, now)?;
            let open_since = store::load_settings(conn)?
                .into_iter()
                .find(|(k, ..)| k == settings::KEY_OPEN_SINCE)
                .and_then(|(_, v, ..)| v.as_i64());
            Ok((active, last_end, open_since))
        });
        let Ok((active, last_end, open_since)) = computed else {
            return;
        };
        let since = last_end
            .unwrap_or(0)
            .max(open_since.unwrap_or(self.started_ms));
        let should_pause = eff.mode == EnrollMode::Open
            && eff.auto_pause_enabled
            && active == 0
            && now - since >= eff.auto_pause_after_ms();
        let was = self.paused.swap(should_pause, Ordering::Relaxed);
        if was == should_pause {
            return;
        }
        let (action, text) = if should_pause {
            *self.paused_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
            (
                "mode.auto_pause",
                format!(
                    "註冊端點：連續 {} 沒有有效的註冊 token，新註冊已自動暫停（過期續簽、輪詢與撤回照常受理；建立 token 後自動恢復）",
                    humanize_ms(eff.auto_pause_after_ms())
                ),
            )
        } else {
            *self.paused_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
            (
                "mode.auto_resume",
                "註冊端點：已有有效的註冊 token，新註冊已恢復".to_string(),
            )
        };
        info!("{text}");
        let _ = self.db.write_blocking(|conn| {
            store::audit(
                conn,
                now,
                &Audit {
                    actor: "system",
                    category: "settings",
                    action,
                    result: "ok",
                    details: Some(json!({"active_tokens": active})),
                    ..Audit::default()
                },
            )?;
            Ok(())
        });
    }

    /// 同步簽發 CA、信任錨與 CRL；有變化時重建 8443 的 TLS 設定
    fn sync_pki(&self, now: i64, startup: bool) {
        let validity_ms = i64::from(self.config.pki.crl_validity_days) * 86_400_000;
        let refresh_ms = i64::from(self.config.pki.crl_refresh_hours) * 3_600_000;
        let result = self.db.write_blocking(|conn| {
            let mut changed = self.pki.sync_cas(conn)?;
            // 需要重新產生 CRL 的 CA：有已生效但未收錄的撤銷、沒有 CRL、nextUpdate 在 7 天內，
            // 或超過 crl_refresh_hours 沒有更新
            let pending: HashSet<i64> =
                crate::pki::store::ca_ids_with_pending_revocations(conn, now)?
                    .into_iter()
                    .collect();
            for ca_id in self.pki.issuer_ids() {
                let latest = crate::pki::store::latest_crl(conn, ca_id)?;
                let due = match &latest {
                    None => true,
                    Some(c) => {
                        c.next_update - now < 7 * 86_400_000 || now - c.this_update >= refresh_ms
                    }
                };
                if !(pending.contains(&ca_id) || due) {
                    continue;
                }
                let source = if pending.contains(&ca_id) {
                    "revocation"
                } else if startup {
                    "startup"
                } else {
                    "scheduled"
                };
                match self
                    .pki
                    .regenerate_crl(conn, ca_id, validity_ms, source, None, now)
                {
                    Ok(Some(n)) => {
                        info!("已產生簽發 CA {ca_id} 的 CRL 第 {n} 版（{source}）");
                        self.last_crl_refresh
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(ca_id, now);
                    }
                    Ok(None) => {}
                    Err(e) => error!("錯誤 KRS-E1205：簽發 CA {ca_id} 的 CRL 產生失敗：{e:?}"),
                }
            }
            changed |= self.pki.write_crl_file(conn)?;
            self.check_issuing_ca_expiry(conn, now);
            Ok(changed)
        });
        match result {
            Ok(true) => self.rebuild_tls(),
            Ok(false) => {}
            Err(e) => warn!("無法同步簽發 CA 與 CRL（稍後重試）：{e}"),
        }
    }

    /// `KRS-W1304` 的門檻（毫秒）：設定值，或「政策有效天數（含有效 token 覆寫中最長者）＋過期寬限期」
    pub fn issuing_ca_warning_ms(&self, conn: &Connection, now: i64) -> i64 {
        if let Some(d) = self.config.pki.issuing_ca_warning_days {
            return i64::from(d) * 86_400_000;
        }
        let eff = self.effective();
        let longest_token = store::list_tokens(conn)
            .map(|ts| {
                ts.iter()
                    .filter(|t| t.state(now) == store::TokenState::Active)
                    .filter_map(|t| t.cert_lifetime_days)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let lifetime = i64::from(eff.policy.cert_lifetime_days).max(longest_token);
        (lifetime + i64::from(eff.policy.expired_renew_grace_days)) * 86_400_000
    }

    /// 啟用中的簽發 CA 剩餘時間少於門檻時記 `KRS-W1304`（每 24 小時最多一次）；回傳 (ca_id, 到期時間)
    fn check_issuing_ca_expiry(&self, conn: &Connection, now: i64) -> Option<(i64, i64)> {
        let ca = crate::pki::store::list_cas(conn)
            .ok()?
            .into_iter()
            .find(|c| c.state == crate::pki::store::CA_ACTIVE)?;
        let not_after = ca.not_after?;
        let threshold = self.issuing_ca_warning_ms(conn, now);
        if not_after - now >= threshold {
            return None;
        }
        let mut last = self
            .last_expiry_warning
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if now - *last >= 86_400_000 {
            *last = now;
            warn!(
                "警告 KRS-W1304：簽發 CA {} 將在 {} 到期（剩 {} 天，門檻 {} 天）：新簽發的憑證會被截短在簽發 CA 到期之前。處理方式：kairos pki issuing-ca new → 以根 CA 簽發 → install → activate（kairos/PKI.md 第 4 節）",
                ca.ca_id,
                crate::api::http::ts_ms(not_after),
                (not_after - now).max(0) / 86_400_000,
                threshold / 86_400_000
            );
        }
        Some((ca.ca_id, not_after))
    }

    /// 重建 8443 的 TLS 設定（信任錨、CRL 改變），並重新驗證現役連線
    pub fn rebuild_tls(&self) {
        crate::reload::trigger().fire();
    }

    /// 簽發已核准的申請（CLI 核准、核准時簽發失敗後重試）
    fn issue_pending(&self, now: i64) {
        if !matches!(self.pki.status(), IssueStatus::Online { .. }) {
            return;
        }
        let eff = self.effective();
        let result = self
            .db
            .write_blocking(|conn| pki_mgr::issue_approved(&self.pki, conn, &eff, now, 200));
        match result {
            Ok((issued, failed)) => {
                for r in &issued {
                    self.stats.issued.fetch_add(1, Ordering::Relaxed);
                    self.set_decommissioned(&r.agent_id, false);
                    info!(
                        "已簽發申請 {}（{}，agent_id {}）的憑證",
                        r.request_code, r.hostname, r.agent_id
                    );
                    self.notify_request(&r.request_id);
                }
                for (id, why) in failed {
                    warn!("申請 {id} 已核准但無法簽發：{why}");
                }
            }
            Err(e) => warn!("無法簽發已核准的申請（稍後重試）：{e}"),
        }
    }

    /// 外部（CLI）改變了長輪詢中申請的狀態：通知
    fn notify_external_changes(&self) {
        let watched: Vec<(String, String)> = {
            let w = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
            w.iter()
                .map(|(k, (_, _, s))| (k.clone(), s.clone()))
                .collect()
        };
        if watched.is_empty() {
            return;
        }
        let ids: Vec<String> = watched.iter().map(|(k, _)| k.clone()).collect();
        let states = self.db.read_blocking(move |conn| {
            let mut out = HashMap::new();
            for id in ids {
                if let Some(r) = store::get_request(conn, &id)? {
                    out.insert(id, r.state.as_str().to_string());
                }
            }
            Ok(out)
        });
        if let Ok(states) = states {
            for (id, before) in watched {
                if states.get(&id).is_some_and(|s| *s != before) {
                    self.notify_request(&id);
                }
            }
        }
    }

    /// 每分鐘：重放快取清除、`KRS-W8302` 彙總、批次部署來源；每小時：申請逾期與保存清除
    fn periodic(&self, now: i64) {
        let mut last_min = self.last_minutely.lock().unwrap_or_else(|e| e.into_inner());
        if now - *last_min >= 60_000 {
            *last_min = now;
            drop(last_min);
            let n = self.stats.clock_since_log.swap(0, Ordering::Relaxed);
            if n > 0 {
                let hosts = self
                    .clock_rejected
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len();
                warn!(
                    "警告 KRS-W8302：過去 1 分鐘有 {n} 個註冊或過期續簽請求因主機時鐘與 Kairos 相差超過 5 分鐘而被拒（最近 15 分鐘 {hosts} 台主機；未消耗 token、未建立申請）。請檢查這些主機的 NTP 設定"
                );
            }
            self.clock_rejected
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, (_, at)| now - *at < CLOCK_REJECTED_KEEP_MS);
            self.queued
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, (_, at)| now - *at < CLOCK_REJECTED_KEEP_MS);
            self.nonces
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, (_, exp)| *exp > now);
            let mut batch = self.config.enroll.batch_sources.clone();
            let extra = self.db.write_blocking(|conn| {
                store::replay_purge(conn, now)?;
                let mut v = store::active_relaxations(conn, now)?;
                v.extend(store::batch_token_cidrs(conn, now)?);
                Ok(v)
            });
            if let Ok(extra) = extra {
                batch.extend(extra);
            }
            if let Ok(list) = Allowlist::parse(&batch) {
                *self.batch.write().unwrap_or_else(|e| e.into_inner()) = list;
            }
            let i = Instant::now();
            self.limits.l4.prune(i);
            self.limits.l4r.prune(i);
            self.limits.l10.prune(i);
        } else {
            drop(last_min);
        }
        let mut last_hour = self.last_hourly.lock().unwrap_or_else(|e| e.into_inner());
        if now - *last_hour >= 3_600_000 {
            *last_hour = now;
            drop(last_hour);
            let audit_keep_ms = i64::from(self.config.audit.retention_days) * 86_400_000;
            let result = self.db.write_blocking(move |conn| {
                let (expired, purged) = store::expire_and_purge(conn, now, CLOSED_KEEP_MS)?;
                store::purge_blocks(conn, now)?;
                store::purge_relaxations(conn, now)?;
                store::purge_audit(conn, now - audit_keep_ms, now)?;
                conn.execute(
                    "DELETE FROM enroll_tokens WHERE coalesce(disabled_at, expires_at) < ?1",
                    [now - 400 * 86_400_000],
                )?;
                Ok((expired, purged))
            });
            match result {
                Ok((e, p)) if e + p > 0 => {
                    info!("註冊申請：{e} 筆待核准申請已逾期，清除 {p} 筆結束超過 30 天的申請")
                }
                Ok(_) => {}
                Err(e) => warn!("註冊申請的保存清除失敗（下一小時重試）：{e}"),
            }
        }
    }

    /// 背景工作（每 [`TICK`] 一次，或被 [`EnrollService::wake`] 喚醒）：設定、自動暫停、簽發、CRL、
    /// 長輪詢通知、listener 的監聽與停止
    pub async fn run(self: Arc<Self>, listener: Arc<server::Listener>) {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let this = self.clone();
            let work = tokio::task::spawn_blocking(move || {
                let now = this.now_ms();
                if let Err(e) = this.db.read_blocking(|c| this.refresh_settings(c)) {
                    debug!("無法讀取註冊設定（沿用目前的值）：{e}");
                }
                this.evaluate_pause(now);
                this.issue_pending(now);
                this.sync_pki(now, false);
                this.notify_external_changes();
                this.periodic(now);
            })
            .await;
            if let Err(e) = work {
                error!("註冊服務的背景工作異常結束（下一輪重試）：{e}");
            }
            listener.reconcile(&self).await;
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = ticker.tick() => {}
            }
        }
        listener.stop().await;
    }

    /// 授權節點數的檢查（本版沒有授權機制：一律通過；`license_exhausted` 保留給之後的版本）
    pub fn license_allows(&self, _additional: usize) -> bool {
        true
    }
}

/// `1 小時`、`24 小時`、`15 分鐘`
pub fn humanize_ms(ms: i64) -> String {
    let secs = ms / 1000;
    if secs % 3600 == 0 && secs >= 3600 {
        format!("{} 小時", secs / 3600)
    } else if secs % 60 == 0 && secs >= 60 {
        format!("{} 分鐘", secs / 60)
    } else {
        format!("{secs} 秒")
    }
}

/// 啟動中的註冊服務（自己的 runtime）
pub struct Running {
    pub service: Arc<EnrollService>,
    task: Option<tokio::task::JoinHandle<()>>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Running {
    /// 關機：停止監聽並等待背景工作結束，再關閉 runtime
    pub async fn join(mut self) {
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

/// 註冊服務 runtime 的工作執行緒數
pub const ENROLL_WORKER_THREADS: usize = 2;
/// blocking 執行緒上限（資料庫、簽章）
pub const ENROLL_BLOCKING_THREADS: usize = 16;

/// 啟動註冊服務：建立 runtime、載入狀態、啟動背景工作（listener 依模式監聽）
pub async fn start(p: StartParams) -> Result<Running, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(ENROLL_WORKER_THREADS)
        .max_blocking_threads(ENROLL_BLOCKING_THREADS)
        .thread_name("kairos-enroll")
        .enable_all()
        .build()
        .map_err(|e| format!("無法建立註冊服務的 runtime：{e}"))?;
    let other = p.other_listens.clone();
    let service = runtime
        .spawn_blocking(move || EnrollService::new(&p))
        .await
        .map_err(|e| format!("註冊服務初始化異常結束：{e}"))??;
    let listener = Arc::new(server::Listener::new(&service, other)?);
    let task = runtime.spawn(service.clone().run(listener.clone()));
    Ok(Running {
        service,
        task: Some(task),
        runtime: Some(runtime),
    })
}

/// `data_dir` 下資料庫的路徑
pub fn db_path(config: &AppConfig, data_dir: &Path) -> PathBuf {
    crate::store::sqlite::SqliteOptions::from_config(config, data_dir).path
}

/// `pki.mode` 的說明（啟動日誌）
pub fn describe_pki(mode: PkiMode) -> &'static str {
    match mode {
        PkiMode::Online => "線上簽發",
        PkiMode::Offline => "離線簽發",
    }
}
