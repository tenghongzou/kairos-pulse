//! 遠端管理（集中設定、Pulse 版本庫、升級推送）的資料庫操作（`RemoteStore`；schema v9，
//! `kairos/STORAGE.md` 第 18 節）。SQLite 的實作在 `store/sqlite/remote.rs`。
//!
//! 交易要求：改變設定版本（`meta.config_rev`）與重新計算期望設定、推送狀態的轉換、匯入與刪除發佈版本，
//! 都由呼叫端在 [`TxMode::Immediate`](crate::store::db::TxMode::Immediate) 的交易中進行（同一個交易中
//! 寫稽核），推送引擎是唯一的寫入者（REMOTE.md 第 4.7.6 節）。
// 查詢結果以結構回傳；方法參數多（與資料表欄位對應）
#![allow(clippy::too_many_arguments)]

#[allow(unused_imports)]
use crate::store::db::StoreResult;

/// `config_profiles` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    /// 兩層 JSON（`{"report":{"interval":10}}`）
    pub settings: String,
    pub revision: i64,
    pub created_at: i64,
    pub created_by: String,
    pub updated_at: i64,
    pub updated_by: String,
}

/// `config_profile_revisions` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRevisionRow {
    pub revision: i64,
    pub settings: String,
    pub description: String,
    pub changed_at: i64,
    pub changed_by: String,
    pub reason: Option<String>,
}

/// `config_assignments` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentRow {
    pub id: i64,
    pub profile_id: i64,
    /// `global`、`group`、`tag`、`host`
    pub selector_kind: String,
    /// `global` 為空字串；`group` 小寫；`host` 為小寫的 agent_id
    pub selector_value: String,
    pub priority: i64,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
}

/// 計算期望設定所需的主機資訊（清冊與 `host_config` 的聯集）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigTarget {
    pub agent_id: String,
    pub host_group: Option<String>,
    /// hello 的 `tags`（`hosts.tags`）
    pub tags: Vec<String>,
    pub agent_version: Option<String>,
    /// hello 的 `install.config`
    pub install_config: Option<String>,
}

/// `host_config` 的一列
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostConfigRow {
    pub agent_id: String,
    pub desired_rev: i64,
    pub desired_hash: String,
    /// 合併後的 settings（兩層 JSON）
    pub desired: String,
    /// 鍵 → 設定檔名稱（JSON 物件）
    pub sources: String,
    pub applied_rev: i64,
    pub applied_hash: String,
    /// `applied`／`rejected`／`rolled_back`／`disabled`；尚未回報為 `None`
    pub state: Option<String>,
    pub effective: Option<String>,
    pub overridden: Option<String>,
    pub ignored: Option<String>,
    pub errors: Option<String>,
    pub code: Option<String>,
    pub reported_at: Option<i64>,
    /// 最近一次 hello 的 `tags`（JSON 陣列）、`agent_version`、`install.config`
    pub tags: Option<String>,
    pub agent_version: Option<String>,
    pub install_config: Option<String>,
}

/// 重新計算後要寫入的期望設定
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredUpdate {
    pub agent_id: String,
    pub desired_rev: i64,
    pub desired_hash: String,
    pub desired: String,
    pub sources: String,
}

/// `config_status` 要寫入的內容（JSON 欄位已序列化）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigReport {
    pub agent_id: String,
    pub rev: i64,
    pub hash: String,
    pub state: String,
    pub effective: String,
    pub overridden: String,
    pub ignored: String,
    pub errors: String,
    pub code: Option<String>,
    pub reported_at: i64,
}

/// `releases` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRow {
    pub id: i64,
    pub version: String,
    pub commit: String,
    /// `hosted`（0.9.0 只有這個）或 `external`（0.10）
    pub source: String,
    pub sha256sums: String,
    pub signature: String,
    /// 匯入時驗證的金鑰指紋（`[remote] allowed_signers` 沒有設定時為 `None`：沒有驗證）
    pub signer_fingerprint: Option<String>,
    pub signer_principal: Option<String>,
    pub manifest_name: Option<String>,
    pub manifest: Option<String>,
    pub imported_at: i64,
    pub imported_by: String,
}

/// `release_artifacts` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRow {
    pub release_id: i64,
    pub filename: String,
    pub os: String,
    pub arch: String,
    pub format: String,
    pub sha256: String,
    pub size: i64,
    /// 檔案已存入版本庫（`<releases_dir>/<sha256>`）
    pub stored: bool,
    pub stored_at: Option<i64>,
}

/// `rollouts` 的一列（`target_version` 取自 `releases`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutRow {
    pub id: String,
    pub name: String,
    pub release_id: i64,
    pub target_version: String,
    /// JSON
    pub selector: String,
    /// JSON
    pub waves: String,
    /// JSON（0.9.0 為 `{}`：立即開始、沒有時段）
    pub schedule: String,
    /// JSON（`max_in_progress`、`health_timeout_secs`、`allow_downgrade`、`allow_no_rollback`）
    pub options: String,
    pub state: String,
    pub current_wave: i64,
    pub wave_started_at: Option<i64>,
    pub soak_until: Option<i64>,
    pub awaiting_continue: bool,
    pub pause_reason: Option<String>,
    /// 自動暫停的原因（`failure_ratio`／`rollback_failed`／`signature_failed`；手動暫停為 `None`）
    pub pause_cause: Option<String>,
    pub cancel_reason: Option<String>,
    pub created_at: i64,
    pub created_by: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub updated_at: i64,
    /// 送出的時間與執行者（0.10.0，schema v10；雙人覆核時核准者不得是建立者或送出者）
    pub submitted_at: Option<i64>,
    pub submitted_by: Option<String>,
    /// 核准的時間與執行者（雙人覆核）
    pub approved_at: Option<i64>,
    pub approved_by: Option<String>,
    /// 最近一次被退回的原因（退回後回到草稿；再次送出時清除）
    pub reject_reason: Option<String>,
}

/// `rollout_hosts` 的一列（`agent_id`、`hostname` 取自清冊）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RolloutHostRow {
    pub rollout_id: String,
    pub host_id: i64,
    pub agent_id: String,
    pub hostname: String,
    pub upgrade_id: String,
    pub wave: i64,
    pub ordinal: i64,
    pub state: String,
    pub reason: Option<String>,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub attempts: i64,
    pub bytes_done: Option<i64>,
    pub bytes_total: Option<i64>,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub offered_at: Option<i64>,
    pub applying_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub updated_at: i64,
    pub acknowledged_at: Option<i64>,
    pub acknowledged_by: Option<String>,
}

/// 推送的資格判斷所需的主機資訊（清冊）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpgradeHost {
    pub host_id: i64,
    pub agent_id: String,
    pub hostname: String,
    pub agent_version: Option<String>,
    /// hello 的 `os`（JSON）
    pub os: Option<String>,
    pub install_method: Option<String>,
    pub install_upgrade: Option<String>,
    pub install_config: Option<String>,
    pub host_group: Option<String>,
    pub tags: Vec<String>,
    pub online: bool,
    pub decommissioned: bool,
}

/// 新增推送對象的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRolloutHost {
    pub host_id: i64,
    pub upgrade_id: String,
    pub wave: i64,
    pub ordinal: i64,
    pub state: String,
    pub reason: Option<String>,
    pub from_version: Option<String>,
}

/// 推送中一台主機的狀態變更（`None` 的欄位不變）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostTransition {
    pub state: Option<String>,
    /// `Some(None)` 清除
    pub reason: Option<Option<String>>,
    pub upgrade_id: Option<String>,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub bump_attempts: bool,
    pub bytes_done: Option<i64>,
    pub bytes_total: Option<i64>,
    pub code: Option<Option<String>>,
    pub detail: Option<Option<String>>,
    pub offered_at: Option<Option<i64>>,
    pub applying_at: Option<Option<i64>>,
    pub finished_at: Option<Option<i64>>,
    pub acknowledged: Option<(i64, String)>,
}

/// 回滾失敗、尚未標記處理的主機：(agent_id, hostname, rollout_id, detail)
pub type RollbackFailure = (String, String, String, Option<String>);

/// 替換中的主機：(agent_id, rollout_id, 進入 `applying` 的時間, 推送的 `options` JSON)
pub type ApplyingHost = (String, String, i64, String);

/// 遠端管理的資料庫操作
pub trait RemoteStore {
    // ---- 設定版本 ----

    /// `meta.config_rev`（沒有時 0）
    fn remote_config_rev(&self) -> StoreResult<i64>;

    /// `meta.config_rev` 加 1，回傳新的值（呼叫端在 Immediate 交易中）
    fn bump_remote_config_rev(&self) -> StoreResult<i64>;

    // ---- 設定檔 ----

    fn list_profiles(&self) -> StoreResult<Vec<ProfileRow>>;

    fn get_profile(&self, id: i64) -> StoreResult<Option<ProfileRow>>;

    /// 名稱（不分大小寫）對應的設定檔 id
    fn profile_id_by_name(&self, name: &str) -> StoreResult<Option<i64>>;

    /// 新增設定檔與第 1 版（`config_profile_revisions`）；回傳 id
    fn insert_profile(
        &self,
        name: &str,
        description: &str,
        settings: &str,
        now_ms: i64,
        by: &str,
        reason: Option<&str>,
    ) -> StoreResult<i64>;

    /// 修改設定檔並新增一版；回傳新的版本號
    fn update_profile(
        &self,
        id: i64,
        name: &str,
        description: &str,
        settings: &str,
        now_ms: i64,
        by: &str,
        reason: Option<&str>,
    ) -> StoreResult<i64>;

    /// 刪除設定檔（含所有版本）；回傳是否存在
    fn delete_profile(&self, id: i64) -> StoreResult<bool>;

    fn profile_revisions(&self, id: i64) -> StoreResult<Vec<ProfileRevisionRow>>;

    // ---- 指派 ----

    fn list_assignments(&self) -> StoreResult<Vec<AssignmentRow>>;

    fn get_assignment(&self, id: i64) -> StoreResult<Option<AssignmentRow>>;

    /// 新增指派；(選擇器, 設定檔) 重複時回傳 `None`
    fn insert_assignment(
        &self,
        profile_id: i64,
        selector_kind: &str,
        selector_value: &str,
        priority: i64,
        expires_at: Option<i64>,
        now_ms: i64,
        by: &str,
    ) -> StoreResult<Option<i64>>;

    fn delete_assignment(&self, id: i64) -> StoreResult<bool>;

    /// 已到期（`expires_at <= now`）的指派
    fn expired_assignments(&self, now_ms: i64) -> StoreResult<Vec<AssignmentRow>>;

    // ---- 每台主機的設定 ----

    /// 清冊與 `host_config` 中所有主機（計算期望設定）
    fn config_targets(&self) -> StoreResult<Vec<ConfigTarget>>;

    /// 一台主機（清冊沒有、`host_config` 也沒有時為 `None`）
    fn config_target(&self, agent_id: &str) -> StoreResult<Option<ConfigTarget>>;

    /// 所有主機目前的 (agent_id, desired_rev, desired_hash)
    fn desired_all(&self) -> StoreResult<Vec<(String, i64, String)>>;

    fn host_config(&self, agent_id: &str) -> StoreResult<Option<HostConfigRow>>;

    fn host_configs(&self) -> StoreResult<Vec<HostConfigRow>>;

    /// 寫入期望設定（沒有這台時新增）
    fn upsert_desired(&self, rows: &[DesiredUpdate]) -> StoreResult<()>;

    /// 記下 hello 的 `tags`（JSON 陣列）、`agent_version` 與 `install.config`（沒有這台時新增）
    fn upsert_hello_info(
        &self,
        agent_id: &str,
        tags: &str,
        agent_version: &str,
        install_config: Option<&str>,
    ) -> StoreResult<()>;

    /// 寫入 `config_status`。`rev` 比已記錄的 `applied_rev` 舊時只更新 `effective`、`overridden`
    /// 與回報時間（不倒退版本）；回傳 (之前的 state, 之前的 overridden 鍵數)
    fn record_config_status(&self, r: &ConfigReport) -> StoreResult<(Option<String>, usize)>;

    // ---- 版本庫 ----

    fn list_releases(&self) -> StoreResult<Vec<ReleaseRow>>;

    fn get_release(&self, version: &str) -> StoreResult<Option<ReleaseRow>>;

    fn get_release_by_id(&self, id: i64) -> StoreResult<Option<ReleaseRow>>;

    fn insert_release(&self, r: &ReleaseRow) -> StoreResult<i64>;

    /// 刪除版本與它的發佈檔紀錄
    fn delete_release(&self, id: i64) -> StoreResult<bool>;

    fn release_artifacts(&self, release_id: i64) -> StoreResult<Vec<ArtifactRow>>;

    fn all_artifacts(&self) -> StoreResult<Vec<ArtifactRow>>;

    /// 新增或更新發佈檔紀錄（`stored`、`stored_at`、大小）
    fn upsert_artifact(&self, a: &ArtifactRow) -> StoreResult<()>;

    /// 其他版本仍參照這個 SHA-256 的檔案數（刪除檔案前檢查）
    fn artifact_refs(&self, sha256: &str) -> StoreResult<i64>;

    // ---- 推送 ----

    /// 清冊中所有主機（推送的資格判斷）
    fn upgrade_hosts(&self) -> StoreResult<Vec<UpgradeHost>>;

    fn upgrade_host(&self, agent_id: &str) -> StoreResult<Option<UpgradeHost>>;

    fn list_rollouts(&self, state: Option<&str>) -> StoreResult<Vec<RolloutRow>>;

    fn get_rollout(&self, id: &str) -> StoreResult<Option<RolloutRow>>;

    /// 以 `r-YYYYMMDD-` 開頭的推送數（產生編號）
    fn count_rollouts_with_prefix(&self, prefix: &str) -> StoreResult<i64>;

    fn insert_rollout(&self, r: &RolloutRow) -> StoreResult<()>;

    /// 修改草稿的內容（名稱、目標、選擇器、批次、選項）
    fn update_rollout_spec(
        &self,
        id: &str,
        name: &str,
        release_id: i64,
        selector: &str,
        waves: &str,
        options: &str,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 修改草稿的排程（`schedule` 的 JSON；0.10）。不是草稿時 `false`
    fn update_rollout_schedule(&self, id: &str, schedule: &str, now_ms: i64) -> StoreResult<bool>;

    /// 修改推送的狀態欄位
    fn update_rollout_state(&self, r: &RolloutRow) -> StoreResult<()>;

    /// 刪除一個推送已固定的對象（已排程的推送被修改而退回草稿時；0.10）；回傳刪除的列數
    fn delete_rollout_hosts(&self, rollout_id: &str) -> StoreResult<i64>;

    /// 刪除已完成或取消、`completed_at` 早於 `before_ms` 的推送（含主機）；回傳刪除的推送數
    fn delete_old_rollouts(&self, before_ms: i64) -> StoreResult<i64>;

    fn insert_rollout_hosts(
        &self,
        rollout_id: &str,
        hosts: &[NewRolloutHost],
        now_ms: i64,
    ) -> StoreResult<()>;

    fn rollout_hosts(&self, rollout_id: &str) -> StoreResult<Vec<RolloutHostRow>>;

    fn rollout_host_by_upgrade_id(&self, upgrade_id: &str) -> StoreResult<Option<RolloutHostRow>>;

    fn rollout_host(&self, rollout_id: &str, agent_id: &str)
    -> StoreResult<Option<RolloutHostRow>>;

    /// 一台主機最近的升級（依更新時間由新到舊）
    fn host_upgrades(&self, agent_id: &str, limit: i64) -> StoreResult<Vec<RolloutHostRow>>;

    fn update_rollout_host(
        &self,
        rollout_id: &str,
        host_id: i64,
        t: &HostTransition,
        now_ms: i64,
    ) -> StoreResult<()>;

    /// 進行中（`draft` 以外、尚未完成或取消）的推送中這些主機的 (agent_id, rollout_id)
    fn hosts_in_active_rollouts(&self) -> StoreResult<Vec<(String, String)>>;

    /// 進行中推送的主機中已提議、尚未替換的 (agent_id, upgrade_id)（啟動時載入）
    fn active_offers(&self) -> StoreResult<Vec<(String, String)>>;

    /// 回滾失敗、尚未標記為已處理的主機：(agent_id, hostname, rollout_id, detail)
    fn unacknowledged_rollback_failures(&self) -> StoreResult<Vec<RollbackFailure>>;

    /// 替換中（`applying`）的主機：(agent_id, rollout_id, 進入 `applying` 的時間, 推送的 `options` JSON)。
    /// 不論推送的狀態（暫停、取消後已交給輔助程式的主機照常完成）；升級中的離線告警抑制用
    fn applying_hosts(&self) -> StoreResult<Vec<ApplyingHost>>;

    /// 使用中（草稿、進行中、暫停）的推送中以這個版本為目標的數量
    fn rollouts_using_release(&self, release_id: i64) -> StoreResult<i64>;
}
