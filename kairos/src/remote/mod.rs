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
pub mod policy;
pub mod schedule;
pub mod store;
pub mod submission;

pub use engine::{ApproveRequest, rollout_json};

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
/// 不能核准自己建立或送出的推送（雙人覆核，REMOTE.md 第 5.4 節；API 403 `self_approval`）
pub const CODE_SELF_APPROVAL: &str = "KRS-E6106";
/// 目標版本與目前的 Kairos 不相容（`min_kairos`）
pub const CODE_INCOMPATIBLE: &str = "KRS-W8415";
/// 升級完成（鏡像 `PLS-I8416`）
pub const CODE_UPGRADED: &str = "KRS-I8416";
/// 版本庫的檔案遺失：已存入的發佈檔不存在或大小不符（系統健康、S25、資格分類 `artifact_missing`、核准時的
/// 409 `artifact_missing`、推送中等待檔案的主機）。ADR-0002 決策點 7／U10：照工程的號碼正式採用（子類別 84
/// 「遠端升級」、只有 Kairos 端、不鏡像；「不存在」與「大小不符」同號）
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

/// 給主控台 SSE 的變化（`config.host`、`rollout.progress`、`rollout.host`；REMOTE.md 第 8.4 節）
#[derive(Debug, Clone)]
pub struct RemoteEvent {
    pub kind: &'static str,
    pub data: serde_json::Value,
}

/// SSE 事件的緩衝（推送的進度每 5 秒最多一則；設定的狀態每台主機每次回報一則）
pub const EVENT_BUFFER: usize = 1024;
/// 一個推送一輪最多送出幾則 `rollout.host`；超過時不逐台送，改在 `rollout.progress` 帶 `hosts_resync: true`
/// （主控台重新抓取主機表），避免大型推送灌滿事件緩衝
pub const ROLLOUT_HOST_EVENTS_MAX: usize = 200;
/// 建議的最低 Pulse 版本多久重新計算一次（`kairos releases` 的 CLI 直接寫資料庫，不會通知服務）
pub const RECOMMENDED_MIN_TTL: Duration = Duration::from_secs(30);
/// 開始訂閱一個推送時，往回補送這段時間內改變過的主機（畫面載入到第一輪之間的變化）
pub const ROLLOUT_WATCH_LOOKBACK_MS: i64 = 15_000;

/// 一台主機上次送出 `rollout.host` 時的樣子（有改變才再送）
type HostMark = (i64, String, Option<i64>, i64, bool);

fn host_mark(h: &store::RolloutHostRow) -> HostMark {
    (
        h.updated_at,
        h.state.clone(),
        h.bytes_done,
        h.attempts,
        h.acknowledged_at.is_some(),
    )
}

/// 主控台 SSE 連線對一個推送的訂閱（`GET /api/v1/events?rollout_id=…`）；放掉時取消
pub struct RolloutWatch {
    svc: Arc<RemoteService>,
    rollout_id: String,
}

impl Drop for RolloutWatch {
    fn drop(&mut self) {
        let mut st = self.svc.lock();
        if let Some(n) = st.watchers.get_mut(&self.rollout_id) {
            *n -= 1;
            if *n == 0 {
                st.watchers.remove(&self.rollout_id);
                st.host_seen.remove(&self.rollout_id);
            }
        }
    }
}

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

/// 「升級中」每隔這段時間重新發布一次（告警引擎漏掉時補上）
pub const UPGRADE_WINDOW_REPUBLISH_MS: i64 = 60_000;

/// 已發布的「升級中」（離線告警抑制）
#[derive(Debug, Clone, PartialEq, Eq)]
struct UpgradeWindow {
    rollout_id: String,
    /// 抑制的期限（[`rollout::suppress_until`]）
    until_ms: i64,
    /// 設定的時間（調和時不解除剛設定的）
    set_ms: i64,
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
    /// 推送 → 訂閱它的 SSE 連線數（有訂閱時才送 `rollout.host`）
    watchers: HashMap<String, usize>,
    /// 訂閱中的推送 → 每台主機上次送出的樣子（還沒有基準時沒有項目）
    host_seen: HashMap<String, HashMap<i64, HostMark>>,
    /// 建議的最低 Pulse 版本的快取（[`RECOMMENDED_MIN_TTL`]）
    recommended_min: Option<(Instant, Option<String>)>,
    /// 已發布給告警引擎、仍有效的系統檢查（規則, 對象）；`None` 為尚未調和過
    alert_keys: Option<HashSet<(&'static str, String)>>,
    /// 已發布給告警引擎的「升級中」（離線告警抑制，REMOTE.md 第 4.7.5 節）：agent_id → 抑制的期限
    upgrade_windows: HashMap<String, UpgradeWindow>,
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
    /// 推送的升級時段沒有指定 `timezone` 時的預設（Kairos 的 `alerts.timezone`；沒有設定時 `+08:00`）
    default_timezone: std::sync::OnceLock<String>,
    /// 啟動時比對過的雙人覆核政策（[`RemoteService::observe_approval_policy`]；CLI 與測試沒有呼叫時為空）
    approval_policy: std::sync::OnceLock<policy::Observed>,
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
            default_timezone: std::sync::OnceLock::new(),
            approval_policy: std::sync::OnceLock::new(),
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

    /// 啟動時比對雙人覆核的政策（[`policy::observe`]；ADR-0002 決策點 2i）：`source` 為 `rollout_approval`
    /// 的來源（`file`／`environment`／`default`）、`node` 為這個節點的名稱。**在遠端管理開始接受請求
    /// （主控台 API、8443 的遠端管理訊息、推送引擎）之前呼叫**；回 `Err` 時呼叫端不得啟動遠端管理——不能在
    /// 沒有記錄的情況下以新的政策接受推送。叢集模式下服務只在取得租約的節點啟動，所以只有 leader 會寫入
    pub async fn observe_approval_policy(
        &self,
        source: &str,
        node: &str,
    ) -> Result<policy::Outcome, String> {
        let now = self.now_ms();
        let value = self.settings.rollout_approval.clone();
        let (source, node) = (source.to_string(), node.to_string());
        let (outcome, record) = self
            .in_tx(move |s| policy::observe(s, &value, &source, &node, now))
            .await
            .map_err(|e| match e {
                ops::TxError::Db(e) => e.to_string(),
                ops::TxError::Api(e) => e.message,
            })?;
        match outcome {
            policy::Outcome::Disabled | policy::Outcome::BaselineDisabled => warn!(
                event = "settings.rollout_approval.change";
                "警告：雙人覆核已關閉（remote.rollout_approval 由 required 改為 none，來源 {}；與資料庫記錄的上一次設定不同）。送出的推送不再需要另一位管理員核准；已寫入稽核並開啟告警事件「雙人覆核已關閉」",
                record.source
            ),
            policy::Outcome::Enabled => info!(
                event = "settings.rollout_approval.change";
                "雙人覆核已啟用（remote.rollout_approval 由 none 改為 required，來源 {}）",
                record.source
            ),
            policy::Outcome::SourceChanged => info!(
                event = "settings.rollout_approval.change";
                "remote.rollout_approval 的來源改變（值仍是 {}，來源 {}）",
                record.value, record.source
            ),
            policy::Outcome::Baseline => info!(
                event = "settings.rollout_approval.baseline";
                "雙人覆核的設定已記錄為基準（remote.rollout_approval = {}，來源 {}）",
                record.value, record.source
            ),
            policy::Outcome::Unchanged => {}
        }
        let _ = self.approval_policy.set(record);
        Ok(outcome)
    }

    /// 啟動時比對過的雙人覆核政策（前值、最近一次改變；沒有比對過時 `None`）
    pub fn approval_policy(&self) -> Option<&policy::Observed> {
        self.approval_policy.get()
    }

    /// 啟動推送引擎與到期指派、保留期限的背景工作
    pub fn spawn_engine(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let svc = self.clone();
        tokio::spawn(async move { svc.run().await })
    }

    pub fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    /// 設定升級時段的預設時區（啟動時以 `alerts.timezone` 呼叫一次；不是固定位移時忽略）
    pub fn set_default_timezone(&self, tz: &str) {
        if schedule::parse_offset_secs(tz).is_some() {
            let _ = self.default_timezone.set(tz.trim().to_string());
        }
    }

    /// 升級時段的預設時區（固定位移）
    pub fn default_timezone(&self) -> String {
        self.default_timezone
            .get()
            .cloned()
            .unwrap_or_else(|| "+08:00".to_string())
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

    /// 建議的最低 Pulse 版本（S04 版本欄的 ⚠；REMOTE.md 第 4.9 節）：版本庫中**最新版本**的清單的
    /// `min_upgrade_from`——低於它的主機無法遠端升級到最新版，需要手動升級。版本庫是空的、最新版本沒有清單
    /// （0.9.0 以前的發佈）或讀不到資料庫時為 `None`（不標示）
    pub async fn recommended_min_agent_version(&self) -> Option<String> {
        if let Some((at, v)) = &self.lock().recommended_min
            && at.elapsed() < RECOMMENDED_MIN_TTL
        {
            return v.clone();
        }
        let v = match self.db.read(|s| Ok(s.list_releases()?)).await {
            Ok(list) => ops::recommended_min_version(&list),
            Err(e) => {
                debug!("遠端管理：無法讀取版本庫（建議的最低版本）：{e}");
                return None;
            }
        };
        self.lock().recommended_min = Some((Instant::now(), v.clone()));
        v
    }

    /// 版本庫改變（匯入、刪除）：下一次重新計算建議的最低版本
    pub(crate) fn forget_recommended_min(&self) {
        self.lock().recommended_min = None;
    }

    /// 主控台 SSE 連線訂閱一個推送的 `rollout.host`（下一輪建立基準並補送最近的變化）
    pub fn watch_rollout(self: &Arc<Self>, rollout_id: &str) -> RolloutWatch {
        {
            let mut st = self.lock();
            *st.watchers.entry(rollout_id.to_string()).or_default() += 1;
            st.dirty_rollouts.insert(rollout_id.to_string());
        }
        self.wake();
        RolloutWatch {
            svc: self.clone(),
            rollout_id: rollout_id.to_string(),
        }
    }

    /// 推送的狀態或主機改變：下一輪送出 `rollout.progress`
    pub(crate) fn mark_dirty(&self, rollout_id: &str) {
        self.lock().dirty_rollouts.insert(rollout_id.to_string());
    }

    /// 送出改變過的推送的 `rollout.progress`（`rollout_id`、`state`、`current_wave`、各狀態的台數、`pause_cause`、
    /// `next_action`），有 SSE 連線訂閱的推送另送每台改變的主機的 `rollout.host`（[`rollout::host_json`]；
    /// 一輪超過 [`ROLLOUT_HOST_EVENTS_MAX`] 台時改在 `rollout.progress` 帶 `hosts_resync: true`）
    pub(crate) async fn flush_progress(&self) {
        let (ids, mut seen) = {
            let mut st = self.lock();
            let ids: Vec<String> = std::mem::take(&mut st.dirty_rollouts).into_iter().collect();
            let seen: HashMap<String, Option<HashMap<i64, HostMark>>> = ids
                .iter()
                .filter(|id| st.watchers.contains_key(*id))
                .map(|id| (id.clone(), st.host_seen.get(id).cloned()))
                .collect();
            (ids, seen)
        };
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
                        let mut v = serde_json::json!({
                            "rollout_id": r.id,
                            "state": r.state,
                            "current_wave": r.current_wave,
                            "progress": rollout::progress(&hosts),
                            "pause_cause": rollout::pause_cause(&r.state, r.pause_cause.as_deref()),
                            "next_action": engine::next_action_of(&r, &hosts, now),
                        });
                        let mut host_events = Vec::new();
                        let mut marks = None;
                        if let Some(before) = seen.remove(&id) {
                            let changed: Vec<&store::RolloutHostRow> = hosts
                                .iter()
                                .filter(|h| match &before {
                                    Some(m) => m.get(&h.host_id) != Some(&host_mark(h)),
                                    None => h.updated_at >= now - ROLLOUT_WATCH_LOOKBACK_MS,
                                })
                                .collect();
                            if changed.len() > ROLLOUT_HOST_EVENTS_MAX {
                                v["hosts_resync"] = serde_json::json!(true);
                                v["hosts_changed"] = serde_json::json!(changed.len());
                            } else {
                                host_events =
                                    changed.iter().map(|h| rollout::host_json(h)).collect();
                            }
                            marks = Some(
                                hosts
                                    .iter()
                                    .map(|h| (h.host_id, host_mark(h)))
                                    .collect::<HashMap<_, _>>(),
                            );
                        }
                        out.push((id, v, host_events, marks));
                    }
                }
                Ok(out)
            })
            .await;
        match rows {
            Ok(list) => {
                for (id, v, host_events, marks) in list {
                    if let Some(m) = marks {
                        let mut st = self.lock();
                        // 這一輪之間取消訂閱的不留基準
                        if st.watchers.contains_key(&id) {
                            st.host_seen.insert(id, m);
                        }
                    }
                    for h in host_events {
                        self.emit("rollout.host", h);
                    }
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
            self.reconcile_upgrade_windows().await;
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
        // 雙人覆核已關閉（ADR-0002 決策點 2i）：啟動時觀察到 `required` → `none`，到改回 `required`（下一次
        // 啟動）為止一直成立——事件不會自己結束
        if let Some(p) = self.approval_policy()
            && let Some(at) = p.disabled_at
        {
            want.insert(
                (ids::ROLLOUT_APPROVAL_DISABLED, "remote".to_string()),
                (
                    "雙人覆核已關閉：升級推送不再需要另一位管理員核准".to_string(),
                    serde_json::json!({
                        "disabled_at": crate::api::http::ts_ms(at),
                        "source": p.source,
                        "node": p.node,
                        "from": p.changed_from,
                        "to": p.value,
                    }),
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

    /// 升級中主機的離線告警抑制（REMOTE.md 第 4.7.5 節）：這台主機進入 `applying`（`Some`：推送與期限）
    /// 或到達終止狀態（`None`）。有改變時才發布給告警引擎
    pub(crate) fn set_upgrade_window(&self, agent_id: &str, w: Option<(String, i64)>) {
        let now = self.now_ms();
        let event = {
            let mut st = self.lock();
            match w {
                Some((rollout_id, until_ms)) if until_ms > now => {
                    // 沒有改變時不重複發布；但每分鐘重送一次（告警服務比遠端管理服務晚啟動、或匯流排落後
                    // 而漏掉時補上，數量最多是替換中的主機數）
                    let same = st.upgrade_windows.get(agent_id).is_some_and(|o| {
                        o.rollout_id == rollout_id
                            && o.until_ms == until_ms
                            && now - o.set_ms < UPGRADE_WINDOW_REPUBLISH_MS
                    });
                    if same {
                        None
                    } else {
                        st.upgrade_windows.insert(
                            agent_id.to_string(),
                            UpgradeWindow {
                                rollout_id: rollout_id.clone(),
                                until_ms,
                                set_ms: now,
                            },
                        );
                        Some((rollout_id, Some(until_ms)))
                    }
                }
                _ => st
                    .upgrade_windows
                    .remove(agent_id)
                    .map(|o| (o.rollout_id, None)),
            }
        };
        if let Some((rollout_id, until_ms)) = event {
            self.registry
                .bus()
                .publish(crate::bus::EventKind::UpgradeWindow {
                    agent_id: agent_id.to_string(),
                    rollout_id,
                    until_ms,
                });
        }
    }

    /// 依資料庫調和「升級中」的主機（`rollout_hosts.state = applying`，不論推送的狀態）：Kairos 重新啟動後
    /// 重建告警引擎的抑制狀態；期限已過或主機已不在 `applying`（例如被推送引擎標為 `unknown`）時解除
    pub(crate) async fn reconcile_upgrade_windows(&self) {
        let rows = match self.db.read(|s| Ok(s.applying_hosts()?)).await {
            Ok(v) => v,
            Err(e) => {
                warn!("升級推送：無法讀取替換中的主機（離線告警抑制）：{e}");
                return;
            }
        };
        let now = self.now_ms();
        let want: HashMap<String, (String, i64)> = rows
            .into_iter()
            .map(|(agent, rollout_id, applying_at, options)| {
                let secs = rollout::options_from_db(&options).health_timeout_secs;
                (
                    agent,
                    (rollout_id, rollout::suppress_until(applying_at, secs)),
                )
            })
            .filter(|(_, (_, until))| *until > now)
            .collect();
        // 剛在 `apply_upgrade_status` 設定、這一輪讀資料庫時可能還沒看到的不解除
        let fresh = ENGINE_INTERVAL.as_millis() as i64;
        let gone: Vec<String> = self
            .lock()
            .upgrade_windows
            .iter()
            .filter(|(a, o)| {
                !want.contains_key(*a) && (o.until_ms <= now || now - o.set_ms >= fresh)
            })
            .map(|(a, _)| a.clone())
            .collect();
        for a in gone {
            self.set_upgrade_window(&a, None);
        }
        for (a, w) in want {
            self.set_upgrade_window(&a, Some(w));
        }
    }

    /// 目前離線告警被抑制的主機數
    #[cfg(test)]
    pub(crate) fn upgrade_windows(&self) -> usize {
        self.lock().upgrade_windows.len()
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
