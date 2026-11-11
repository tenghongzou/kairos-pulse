//! 遠端管理（商業審計 F8）：集中設定（K1）與 Pulse 遠端升級（K2）的 Kairos 端。規格
//! [`crates/kairos-proto/REMOTE.md`](../../../crates/kairos-proto/REMOTE.md)，協定以 PROTOCOL.md
//! 「遠端管理」為準，實作附註見 REMOTE.md 第 13.1 節。
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`store`] | 資料表的操作（`RemoteStore`，schema v9） |
//! | [`desired`] | 設定檔、指派、合併、版本與雜湊、衝突、白名單 |
//! | [`releases`] | 版本庫：匯入與驗證、發佈檔的保存與讀取 |
//! | [`rollout`] | 推送的內容、資格分類、推送引擎的一步（純函式） |
//! | [`ops`] | 主控台 API 與 CLI 的寫入操作（交易、稽核、重新計算） |
//! | [`agent`] | 8443 連線上的訊息處理（`config_get`、`config_status`、`upgrade_get`、`upgrade_chunk_get`、`upgrade_status`） |
//!
//! [`RemoteService`] 在 SQLite 儲存時啟動，持有自己的資料庫連線（與註冊服務相同的做法：一條寫入連線、
//! 數條唯讀連線）與記憶體中的狀態：每台主機的 `desired_rev`、提議中的 `upgrade_id`（心跳的 `pending` 不查
//! 資料庫）、協商了 `upgrade` 的連線、下載名額。推送引擎在服務中執行（單一寫入者，REMOTE.md 第 4.7.6 節）。

pub mod agent;
pub mod desired;
mod engine;
pub mod ops;
pub mod releases;
pub mod rollout;
pub mod store;

pub use engine::rollout_json;

#[cfg(test)]
pub(crate) mod test_sign;
#[cfg(test)]
mod tests;

use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use crate::config::RemoteSettings;
use crate::enroll::db::{DbError, EnrollDb};
use crate::registry::Registry;
use crate::shutdown::Shutdown;
use crate::store::db::{Store, TxMode};
use desired::ConfigSnapshot;

// ---- 產品代碼（設計包 decisions/INTEGRATION.md §31.3；copywriting.md §3.3） ----

/// 下發的設定被 Pulse 拒絕（鏡像 `PLS-W0501`）
pub const CODE_CONFIG_REJECTED: &str = "KRS-W0501";
/// 設定檔的內容不合法（API 400 `config_invalid`）
pub const CODE_CONFIG_INVALID: &str = "KRS-E0502";
/// 下發的設定有鍵被本機覆蓋（鏡像 `PLS-I0503`）
pub const CODE_CONFIG_OVERRIDDEN: &str = "KRS-I0503";
/// 套用後確認失敗，已回復上一份（鏡像 `PLS-W0504`）
pub const CODE_CONFIG_ROLLED_BACK: &str = "KRS-W0504";
/// 集中設定在主機上停用（鏡像 `PLS-I0505`）
pub const CODE_CONFIG_DISABLED: &str = "KRS-I0505";
/// 發佈包的簽章驗證失敗（Pulse 回報 `PLS-E8401`）
pub const CODE_SIGNATURE_FAILED: &str = "KRS-E8401";
/// 發佈包與提議不符；升級後的版本與目標不符
pub const CODE_VERSION_MISMATCH: &str = "KRS-W8402";
/// 回滾失敗，需要人工處理（鏡像 `PLS-E8408`）
pub const CODE_ROLLBACK_FAILED: &str = "KRS-E8408";
/// 已自動回滾（鏡像 `PLS-W8407`）
pub const CODE_ROLLED_BACK: &str = "KRS-W8407";
/// 匯入發佈版本失敗
pub const CODE_IMPORT_FAILED: &str = "KRS-E8413";
/// 推送已自動暫停
pub const CODE_ROLLOUT_PAUSED: &str = "KRS-W8414";
/// 目標版本與目前的 Kairos 不相容（`min_kairos`）
pub const CODE_INCOMPATIBLE: &str = "KRS-W8415";
/// 升級完成（鏡像 `PLS-I8416`）
pub const CODE_UPGRADED: &str = "KRS-I8416";
/// 版本庫的檔案遺失：已存入的發佈檔不存在或大小不符（系統健康；〔暫定〕號碼，待設計包正式分配）
pub const CODE_RELEASE_FILE_MISSING: &str = "KRS-W8417";

/// Pulse 的代碼換成鏡像的 Kairos 代碼（同號；`KRS-W8402` 與 `PLS-E8402` 等級不同）
pub fn mirror_code(pulse_code: &str) -> Option<&'static str> {
    Some(match pulse_code.strip_prefix("PLS-")?.get(1..)? {
        "0501" => CODE_CONFIG_REJECTED,
        "0503" => CODE_CONFIG_OVERRIDDEN,
        "0504" => CODE_CONFIG_ROLLED_BACK,
        "0505" => CODE_CONFIG_DISABLED,
        "8401" => CODE_SIGNATURE_FAILED,
        "8402" => CODE_VERSION_MISMATCH,
        "8404" => "KRS-W8404",
        "8405" => "KRS-E8405",
        "8406" => "KRS-E8406",
        "8407" => CODE_ROLLED_BACK,
        "8408" => CODE_ROLLBACK_FAILED,
        "8409" => "KRS-I8409",
        "8410" => "KRS-W8410",
        "8411" => "KRS-W8411",
        "8412" => "KRS-W8412",
        "8416" => CODE_UPGRADED,
        _ => return None,
    })
}

/// 推送引擎的間隔（有狀態變化時另外喚醒）
pub const ENGINE_INTERVAL: Duration = Duration::from_secs(5);
/// 到期指派的檢查間隔
pub const EXPIRY_INTERVAL: Duration = Duration::from_secs(60);
/// 推送保留期限的清除間隔
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);
/// 下載名額不足時的 `retry_after_ms`（Pulse 另加 ±20% 抖動）
pub const DOWNLOAD_RETRY_AFTER_MS: u32 = 15_000;

/// 給主控台 SSE 的變化（`config.host`、`rollout.progress`；REMOTE.md 第 8.4 節）
#[derive(Debug, Clone)]
pub struct RemoteEvent {
    pub kind: &'static str,
    pub data: serde_json::Value,
}

/// SSE 事件的緩衝（推送的進度每 5 秒最多一則；設定的狀態每台主機每次回報一則）
pub const EVENT_BUFFER: usize = 1024;

/// 這條連線上協商的遠端管理與主機資訊（hello 時記下，推送引擎與提議使用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub conn_id: u64,
    pub agent_version: String,
    pub install_method: Option<String>,
    pub install_upgrade: Option<String>,
    pub arch: Option<String>,
    pub config: bool,
    pub upgrade: bool,
}

/// 提議中的檔案（`upgrade_chunk_get` 只接受這些）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferFiles {
    pub agent_id: String,
    /// (檔名, SHA-256, 大小)
    pub files: Vec<(String, String, u64)>,
}

#[derive(Debug, Default)]
struct State {
    /// agent_id → desired_rev（心跳的 `pending.config_rev`）
    desired: HashMap<String, i64>,
    /// agent_id → 提議中（`offered`／`accepted`／`staged`）的 upgrade_id
    offers: HashMap<String, String>,
    /// 連線中、協商了 `config` 或 `upgrade` 的主機
    sessions: HashMap<String, AgentSession>,
    /// upgrade_id → 提議中的檔案
    offer_files: HashMap<String, Arc<OfferFiles>>,
    /// 下載中的主機 → 最後一次請求的時間
    downloads: HashMap<String, Instant>,
    /// 狀態改變、尚未送出 `rollout.progress` 的推送
    dirty_rollouts: HashSet<String>,
    /// 已發布給告警引擎、仍有效的系統檢查（規則, 對象）；`None` 為尚未調和過
    alert_keys: Option<HashSet<(&'static str, String)>>,
}

/// [`RemoteService::start`] 的參數
pub struct StartParams {
    pub settings: RemoteSettings,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub registry: Arc<Registry>,
    pub shutdown: Arc<Shutdown>,
    pub clock: crate::enroll::Clock,
}

/// 遠端管理的服務
pub struct RemoteService {
    pub settings: RemoteSettings,
    pub data_dir: PathBuf,
    pub releases_dir: PathBuf,
    pub db: Arc<EnrollDb>,
    pub registry: Arc<Registry>,
    pub shutdown: Arc<Shutdown>,
    clock: crate::enroll::Clock,
    /// 服務啟動的時間（Unix 毫秒；推送引擎在啟動後 1 分鐘內不提議新的主機）
    pub started_ms: i64,
    state: Mutex<State>,
    snapshot: RwLock<Arc<ConfigSnapshot>>,
    wake: Notify,
    events: tokio::sync::broadcast::Sender<RemoteEvent>,
}

impl std::fmt::Debug for RemoteService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteService")
            .field("releases_dir", &self.releases_dir)
            .finish_non_exhaustive()
    }
}

/// 資料庫錯誤的文字（日誌）
pub(crate) fn db_err(e: crate::store::db::StoreError) -> DbError {
    DbError::from(e)
}

impl RemoteService {
    /// 開啟資料庫、載入記憶體中的狀態（不啟動推送引擎：[`RemoteService::spawn_engine`]）
    pub async fn open(p: StartParams) -> Result<Arc<RemoteService>, String> {
        let releases_dir = p.settings.releases_path(&p.data_dir);
        releases::ensure_dir(&releases_dir)
            .map_err(|e| format!("無法建立版本庫目錄 {}：{e}", releases_dir.display()))?;
        let db = EnrollDb::new(p.db_path);
        let (desired, offers, snapshot) = db
            .read(|s| {
                let desired: HashMap<String, i64> = s
                    .desired_all()?
                    .into_iter()
                    .map(|(a, rev, _)| (a, rev))
                    .collect();
                let offers: HashMap<String, String> = s.active_offers()?.into_iter().collect();
                let snapshot = ConfigSnapshot::load(s)?;
                Ok((desired, offers, snapshot))
            })
            .await
            .map_err(|e| format!("無法載入集中設定與推送的狀態：{e}"))?;
        let now = (p.clock)();
        Ok(Arc::new(RemoteService {
            settings: p.settings,
            data_dir: p.data_dir,
            releases_dir,
            db,
            registry: p.registry,
            shutdown: p.shutdown,
            clock: p.clock,
            started_ms: now,
            state: Mutex::new(State {
                desired,
                offers,
                ..Default::default()
            }),
            snapshot: RwLock::new(Arc::new(snapshot)),
            wake: Notify::new(),
            events: tokio::sync::broadcast::channel(EVENT_BUFFER).0,
        }))
    }

    /// 啟動推送引擎與到期指派、保留期限的背景工作
    pub fn spawn_engine(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let svc = self.clone();
        tokio::spawn(async move { svc.run().await })
    }

    pub fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 目前的設定檔與指派的快照
    pub fn snapshot(&self) -> Arc<ConfigSnapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn set_snapshot(&self, snap: ConfigSnapshot) {
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(snap);
    }

    /// 喚醒推送引擎（狀態改變後）
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// 主控台 SSE 訂閱遠端管理的變化
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RemoteEvent> {
        self.events.subscribe()
    }

    pub(crate) fn emit(&self, kind: &'static str, data: serde_json::Value) {
        // 沒有訂閱者（主控台停用）時忽略
        let _ = self.events.send(RemoteEvent { kind, data });
    }

    /// 推送的狀態或主機改變：下一輪送出 `rollout.progress`
    pub(crate) fn mark_dirty(&self, rollout_id: &str) {
        self.lock().dirty_rollouts.insert(rollout_id.to_string());
    }

    /// 送出改變過的推送的 `rollout.progress`（`rollout_id`、`state`、`current_wave`、各狀態的台數、`pause_cause`、
    /// `next_action`）
    pub(crate) async fn flush_progress(&self) {
        let ids: Vec<String> = std::mem::take(&mut self.lock().dirty_rollouts)
            .into_iter()
            .collect();
        if ids.is_empty() || self.events.receiver_count() == 0 {
            return;
        }
        let now = self.now_ms();
        let rows = self
            .db
            .read(move |s| {
                let mut out = Vec::new();
                for id in ids {
                    if let Some(r) = s.get_rollout(&id)? {
                        let hosts = s.rollout_hosts(&id)?;
                        out.push(serde_json::json!({
                            "rollout_id": r.id,
                            "state": r.state,
                            "current_wave": r.current_wave,
                            "progress": rollout::progress(&hosts),
                            "pause_cause": rollout::pause_cause(&r.state, r.pause_cause.as_deref()),
                            "next_action": rollout::next_action(&r.state, r.soak_until, r.awaiting_continue, r.pause_reason.as_deref(), r.pause_cause.as_deref(), now),
                        }));
                    }
                }
                Ok(out)
            })
            .await;
        match rows {
            Ok(list) => {
                for v in list {
                    self.emit("rollout.progress", v);
                }
            }
            Err(e) => warn!("升級推送：無法讀取推送的進度（SSE）：{e}"),
        }
    }

    // ---- 記憶體中的狀態 ----

    /// 重新計算後更新 `desired_rev`
    pub(crate) fn apply_desired(&self, changed: &[(String, i64)]) {
        let mut st = self.lock();
        for (a, rev) in changed {
            st.desired.insert(a.clone(), *rev);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn desired_rev(&self, agent_id: &str) -> i64 {
        self.lock().desired.get(agent_id).copied().unwrap_or(0)
    }

    /// 這台主機提議中的 upgrade_id
    pub fn offer_of(&self, agent_id: &str) -> Option<String> {
        self.lock().offers.get(agent_id).cloned()
    }

    pub(crate) fn set_offer(&self, agent_id: &str, upgrade_id: Option<&str>) {
        let mut st = self.lock();
        match upgrade_id {
            Some(id) => {
                st.offers.insert(agent_id.to_string(), id.to_string());
            }
            None => {
                if let Some(old) = st.offers.remove(agent_id) {
                    st.offer_files.remove(&old);
                }
                st.downloads.remove(agent_id);
            }
        }
    }

    /// 心跳與 `hello_ack` 的 `pending`
    pub fn pending(
        &self,
        session: kairos_proto::mgmt::MgmtSession,
        agent_id: &str,
    ) -> Option<kairos_proto::mgmt::Pending> {
        let (rev, offer) = {
            let st = self.lock();
            (
                st.desired.get(agent_id).copied().unwrap_or(0),
                st.offers.get(agent_id).cloned(),
            )
        };
        kairos_proto::mgmt::Pending::for_session(session, rev.max(0) as u64, offer.as_deref())
    }

    /// 連線協商了遠端管理（hello 之後）
    pub fn attach(&self, agent_id: &str, s: AgentSession) {
        let upgrade = s.upgrade;
        self.lock().sessions.insert(agent_id.to_string(), s);
        if upgrade {
            self.wake();
        }
    }

    /// 連線結束（`conn_id` 不符時表示已被新連線取代，不移除）
    pub fn detach(&self, agent_id: &str, conn_id: u64) {
        let mut st = self.lock();
        if st
            .sessions
            .get(agent_id)
            .is_some_and(|s| s.conn_id == conn_id)
        {
            st.sessions.remove(agent_id);
        }
    }

    /// 連線中的主機（推送的資格判斷以 hello 的資訊為準）
    pub(crate) fn sessions_snapshot(&self) -> HashMap<String, AgentSession> {
        self.lock().sessions.clone()
    }

    pub fn session(&self, agent_id: &str) -> Option<AgentSession> {
        self.lock().sessions.get(agent_id).cloned()
    }

    pub(crate) fn capable_agents(&self) -> HashSet<String> {
        self.lock()
            .sessions
            .iter()
            .filter(|(_, s)| s.upgrade)
            .map(|(a, _)| a.clone())
            .collect()
    }

    pub(crate) fn cached_files(&self, upgrade_id: &str) -> Option<Arc<OfferFiles>> {
        self.lock().offer_files.get(upgrade_id).cloned()
    }

    pub(crate) fn cache_files(&self, upgrade_id: &str, files: Arc<OfferFiles>) {
        self.lock()
            .offer_files
            .insert(upgrade_id.to_string(), files);
    }

    /// 取得下載名額：已在下載中的主機延續；名額已滿時回 `false`（`rate_limited`）。超過 10 分鐘沒有
    /// 請求下一段的主機釋出名額
    pub fn acquire_download(&self, agent_id: &str) -> bool {
        let idle = Duration::from_secs(kairos_proto::mgmt::DOWNLOAD_IDLE_RELEASE_SECS);
        let now = Instant::now();
        let mut st = self.lock();
        st.downloads.retain(|_, t| now.duration_since(*t) < idle);
        if let Some(t) = st.downloads.get_mut(agent_id) {
            *t = now;
            return true;
        }
        if st.downloads.len() >= self.settings.max_concurrent_downloads as usize {
            return false;
        }
        st.downloads.insert(agent_id.to_string(), now);
        true
    }

    pub fn release_download(&self, agent_id: &str) {
        self.lock().downloads.remove(agent_id);
    }

    /// 目前下載中的主機數（系統健康）
    pub fn downloads_in_progress(&self) -> usize {
        let idle = Duration::from_secs(kairos_proto::mgmt::DOWNLOAD_IDLE_RELEASE_SECS);
        let now = Instant::now();
        self.lock()
            .downloads
            .values()
            .filter(|t| now.duration_since(**t) < idle)
            .count()
    }

    // ---- 背景工作 ----

    async fn run(self: Arc<Self>) {
        let mut last_expiry = Instant::now() - EXPIRY_INTERVAL;
        let mut last_retention = Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(ENGINE_INTERVAL) => {}
            }
            if last_expiry.elapsed() >= EXPIRY_INTERVAL {
                last_expiry = Instant::now();
                if let Err(e) = self.expire_assignments().await {
                    warn!("集中設定：無法處理到期的指派：{e}");
                }
            }
            if last_retention.elapsed() >= RETENTION_INTERVAL {
                last_retention = Instant::now();
                self.purge_old_rollouts().await;
            }
            if let Err(e) = self.tick().await {
                warn!("升級推送：推送引擎這一輪失敗（稍後重試）：{e}");
            }
            self.flush_progress().await;
            self.reconcile_alerts().await;
        }
        debug!("遠端管理的背景工作結束");
    }

    /// 告警的系統規則（REMOTE.md 第 4.7.4 節）：依資料庫的狀態調和「升級推送已暫停」（自動暫停的推送）、
    /// 「發佈簽章驗證失敗」（因簽章失敗而暫停的推送的目標版本）、「升級回滾失敗」（未標記處理的主機），
    /// 只發布改變的部分（第一次全部發布：Kairos 重新啟動後重建告警引擎的狀態）
    pub(crate) async fn reconcile_alerts(&self) {
        use crate::alerts::rules::ids;
        let rows = self
            .db
            .read(|s| {
                Ok((
                    s.list_rollouts(Some(rollout::ROLLOUT_PAUSED))?,
                    s.unacknowledged_rollback_failures()?,
                ))
            })
            .await;
        let (paused, failures) = match rows {
            Ok(v) => v,
            Err(e) => {
                warn!("升級推送：無法讀取告警的狀態：{e}");
                return;
            }
        };
        let mut want: HashMap<(&'static str, String), (String, serde_json::Value)> = HashMap::new();
        for r in paused.iter().filter(|r| r.pause_cause.is_some()) {
            let cause = r.pause_cause.clone().unwrap_or_default();
            want.insert(
                (ids::ROLLOUT_PAUSED, r.id.clone()),
                (
                    format!("推送「{}」已自動暫停：{}", r.name, r.pause_reason.as_deref().unwrap_or("")),
                    serde_json::json!({"rollout_id": r.id, "name": r.name, "target_version": r.target_version, "reason": r.pause_reason, "cause": cause}),
                ),
            );
            if cause == rollout::PAUSE_SIGNATURE_FAILED {
                want.insert(
                    (ids::RELEASE_SIGNATURE_FAILED, r.target_version.clone()),
                    (
                        format!(
                            "Pulse {} 發佈包的簽章驗證失敗，使用這個版本的推送已暫停",
                            r.target_version
                        ),
                        serde_json::json!({"target_version": r.target_version, "rollout_id": r.id}),
                    ),
                );
            }
        }
        for (agent, host, rid, detail) in failures {
            want.insert(
                (ids::UPGRADE_ROLLBACK_FAILED, agent.clone()),
                (
                    format!("升級回滾失敗：{host} 需要人工處理"),
                    serde_json::json!({"agent_id": agent, "hostname": host, "rollout_id": rid, "detail": detail}),
                ),
            );
        }
        let (added, removed) = {
            let mut st = self.lock();
            let first = st.alert_keys.is_none();
            let old = st.alert_keys.take().unwrap_or_default();
            let now: HashSet<(&'static str, String)> = want.keys().cloned().collect();
            let added: Vec<_> = now
                .iter()
                .filter(|k| first || !old.contains(*k))
                .cloned()
                .collect();
            let removed: Vec<_> = old.difference(&now).cloned().collect();
            st.alert_keys = Some(now);
            (added, removed)
        };
        let bus = self.registry.bus();
        for key in added {
            let (title, details) = want.remove(&key).unwrap_or_default();
            bus.publish(crate::bus::EventKind::RemoteCondition {
                rule: key.0,
                subject: key.1,
                active: true,
                title,
                details,
            });
        }
        for key in removed {
            bus.publish(crate::bus::EventKind::RemoteCondition {
                rule: key.0,
                subject: key.1,
                active: false,
                title: String::new(),
                details: serde_json::Value::Null,
            });
        }
    }

    /// 刪除超過 `rollout_retention_days` 的已結束推送
    async fn purge_old_rollouts(&self) {
        let before = self.now_ms() - i64::from(self.settings.rollout_retention_days) * 86_400_000;
        match self
            .db
            .write(move |s| s.delete_old_rollouts(before).map_err(db_err))
            .await
        {
            Ok(0) => {}
            Ok(n) => info!("升級推送：已刪除 {n} 個超過保留期限的推送紀錄"),
            Err(e) => warn!("升級推送：無法刪除過期的推送紀錄：{e}"),
        }
    }

    /// 在 Immediate 交易中執行 `f`（寫入連線）。`f` 回錯誤時交易回復：資料庫錯誤為 [`ops::TxError::Db`]，
    /// 要回給呼叫端的錯誤（404、409…）為 [`ops::TxError::Api`]
    pub(crate) async fn in_tx<R, F>(&self, f: F) -> Result<R, ops::TxError>
    where
        R: Send + 'static,
        F: FnOnce(&dyn Store) -> Result<R, ops::TxError> + Send + 'static,
    {
        let r = self
            .db
            .write(move |c| {
                let tx = c.begin(TxMode::Immediate)?;
                match f(&*tx) {
                    Ok(r) => {
                        tx.commit()?;
                        Ok(Ok(r))
                    }
                    Err(ops::TxError::Db(e)) => Err(e),
                    // 交易在 drop 時回復
                    Err(other) => Ok(Err(other)),
                }
            })
            .await;
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(ops::TxError::Db(e)),
        }
    }
}
