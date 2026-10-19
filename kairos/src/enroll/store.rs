//! 註冊相關資料表的讀寫（schema v4，`kairos/PKI.md` 第 7 節）：`enroll_tokens`、`enroll_requests`、
//! `enroll_replay`、`enroll_blocks`、`enroll_relaxations`、`settings`、`audit_events`，以及 `hosts` 中與
//! 註冊有關的欄位。
//!
//! 所有函式都是同步的；常駐行程在 blocking 執行緒上呼叫，CLI 直接呼叫。需要原子性的多步操作由呼叫端包在
//! 交易中（`rusqlite::Transaction` 會 deref 成 `Connection`）。

use crate::store::db::StoreResult;
use kairos_proto::enroll::{ApprovalDetail, RequestKind, RequestState, TokenApproval};
use serde_json::Value;

/// `enroll_requests.state`（Kairos 內部比協定多一個 `approved`：已核准、尚未簽發）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbState {
    PendingApproval,
    Approved,
    Issued,
    Rejected,
    RequestExpired,
    Withdrawn,
    Replaced,
}

impl DbState {
    pub const ALL: [DbState; 7] = [
        DbState::PendingApproval,
        DbState::Approved,
        DbState::Issued,
        DbState::Rejected,
        DbState::RequestExpired,
        DbState::Withdrawn,
        DbState::Replaced,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DbState::PendingApproval => "pending_approval",
            DbState::Approved => "approved",
            DbState::Issued => "issued",
            DbState::Rejected => "rejected",
            DbState::RequestExpired => "request_expired",
            DbState::Withdrawn => "withdrawn",
            DbState::Replaced => "replaced",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == s)
    }

    /// 協定上的狀態與 `approval_detail`
    pub fn protocol(self) -> (RequestState, Option<ApprovalDetail>) {
        match self {
            DbState::PendingApproval => (
                RequestState::PendingApproval,
                Some(ApprovalDetail::AwaitingApproval),
            ),
            DbState::Approved => (
                RequestState::PendingApproval,
                Some(ApprovalDetail::ApprovedAwaitingIssuance),
            ),
            DbState::Issued => (RequestState::Issued, None),
            DbState::Rejected => (RequestState::Rejected, None),
            DbState::RequestExpired => (RequestState::RequestExpired, None),
            DbState::Withdrawn => (RequestState::Withdrawn, None),
            DbState::Replaced => (RequestState::Replaced, None),
        }
    }

    /// 仍在待核准佇列中（L9 計數、可撤回、可核准）
    pub fn is_pending(self) -> bool {
        matches!(self, DbState::PendingApproval | DbState::Approved)
    }
}

/// `enroll_requests.kind`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbKind {
    Enroll,
    GraceRenewal,
    /// 8443 的正常輪替（政策要求人工核准時才建立申請）
    Renewal,
}

impl DbKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DbKind::Enroll => "enroll",
            DbKind::GraceRenewal => "grace_renewal",
            DbKind::Renewal => "renewal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [DbKind::Enroll, DbKind::GraceRenewal, DbKind::Renewal]
            .into_iter()
            .find(|k| k.as_str() == s)
    }

    /// 協定上的種類（8443 輪替不會出現在 8444 的回應中，以 `grace_renewal` 表示）
    pub fn protocol(self) -> RequestKind {
        match self {
            DbKind::Enroll => RequestKind::Enroll,
            DbKind::GraceRenewal | DbKind::Renewal => RequestKind::GraceRenewal,
        }
    }
}

pub(crate) fn json_list(v: Option<String>) -> Option<Vec<String>> {
    v.and_then(|s| serde_json::from_str(&s).ok())
}

pub(crate) fn list_json(v: &Option<Vec<String>>) -> Option<String> {
    v.as_ref().map(|l| Value::from(l.clone()).to_string())
}

// ---------------------------------------------------------------------------
// 註冊 token
// ---------------------------------------------------------------------------

/// `enroll_tokens` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRow {
    pub token_id: String,
    pub secret_hash: Vec<u8>,
    pub hint: String,
    pub name: String,
    pub group_name: String,
    pub tags: Option<Vec<String>>,
    pub approval: TokenApproval,
    pub max_uses: i64,
    pub used: i64,
    pub expires_at: i64,
    pub allowed_cidrs: Option<Vec<String>>,
    pub batch_mode: bool,
    pub allowed_agent_ids: Option<Vec<String>>,
    pub cert_lifetime_days: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
    pub disabled_at: Option<i64>,
    pub disabled_by: Option<String>,
    pub disable_reason: Option<String>,
    pub last_used_at: Option<i64>,
}

/// token 的狀態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    Active,
    Expired,
    Exhausted,
    Disabled,
}

impl TokenState {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenState::Active => "active",
            TokenState::Expired => "expired",
            TokenState::Exhausted => "exhausted",
            TokenState::Disabled => "disabled",
        }
    }
}

impl TokenRow {
    pub fn state(&self, now_ms: i64) -> TokenState {
        if self.disabled_at.is_some() {
            TokenState::Disabled
        } else if self.expires_at <= now_ms {
            TokenState::Expired
        } else if self.used >= self.max_uses {
            TokenState::Exhausted
        } else {
            TokenState::Active
        }
    }

    pub fn remaining_uses(&self) -> u32 {
        u32::try_from((self.max_uses - self.used).max(0)).unwrap_or(u32::MAX)
    }
}

pub fn approval_str(a: TokenApproval) -> &'static str {
    match a {
        TokenApproval::Manual => "manual",
        TokenApproval::Auto => "auto",
    }
}

// ---------------------------------------------------------------------------
// 申請
// ---------------------------------------------------------------------------

/// `enroll_requests` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub request_id: String,
    pub kind: DbKind,
    pub agent_id: String,
    pub request_code: String,
    pub state: DbState,
    pub csr_der: Vec<u8>,
    pub csr_sha256: Vec<u8>,
    pub spki_sha256: Vec<u8>,
    pub poll_secret_hash: Option<Vec<u8>>,
    pub token_id: Option<String>,
    pub expired_serial: Option<String>,
    pub previous_serial: Option<String>,
    pub source_ip: String,
    pub hostname: String,
    pub display_name: Option<String>,
    pub tags: Option<Vec<String>>,
    pub os: Option<String>,
    pub host_fingerprint: Option<String>,
    pub pulse_version: Option<String>,
    pub server_verification: Option<String>,
    pub flags: Vec<String>,
    pub cert_lifetime_days: Option<i64>,
    pub created_at: i64,
    pub expires_at: i64,
    pub decided_at: Option<i64>,
    pub decided_by: Option<String>,
    pub reject_reason_code: Option<String>,
    pub reject_reason: Option<String>,
    pub issued_serial: Option<String>,
    pub issue_error: Option<String>,
    pub delivered_at: Option<i64>,
    pub withdrawn_at: Option<i64>,
    pub withdraw_reason: Option<String>,
    pub withdraw_initiated_by: Option<String>,
    pub withdraw_ip: Option<String>,
    pub closed_at: Option<i64>,
}

/// 列表的篩選
#[derive(Debug, Clone, Default)]
pub struct RequestFilter {
    /// `true`：最近結束；`false`：待核准（含已核准待簽發）
    pub closed: bool,
    pub token_id: Option<String>,
    pub kind: Option<DbKind>,
    pub state: Option<DbState>,
    pub limit: usize,
}

/// L7 封鎖的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRow {
    pub source: String,
    pub class: String,
    pub blocked_until: i64,
    pub offenses: i64,
    pub last_offense_at: i64,
}

/// 稽核事件（`audit_events`）：結構與雜湊鏈在 [`crate::audit`]
pub use crate::audit::Audit;

// ---------------------------------------------------------------------------
// hosts（清冊）中與註冊有關的欄位
// ---------------------------------------------------------------------------

/// 清冊中與註冊、續簽、除役有關的欄位
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEnroll {
    pub agent_id: String,
    pub hostname: String,
    pub host_fingerprint: Option<String>,
    pub lifecycle: String,
    pub decommissioned_at: Option<i64>,
    pub current_serial: Option<String>,
    pub grace_override_until: Option<i64>,
    pub last_seen: i64,
    pub online: bool,
}

/// 註冊通道的儲存：`enroll_tokens`、`enroll_requests`、`enroll_replay`、`enroll_blocks`、`enroll_relaxations`，
/// 以及 `hosts` 中與註冊有關的欄位。SQLite 的實作在 `store/sqlite/enroll.rs`
///
/// 需要原子性的多步操作由呼叫端包在交易中（[`crate::store::db::Store::begin`]）。`kairos/HA.md` 第 5.4 節中需要
/// 序列化的操作一律在 [`TxMode::Immediate`](crate::store::db::TxMode::Immediate) 的交易中呼叫：
///
/// - D2 token 扣次數：先 [`EnrollStore::get_token`]（或 [`EnrollStore::token_by_hash`]）檢查剩餘次數，再
///   [`EnrollStore::consume_token`]，與建立申請（[`EnrollStore::insert_request`]）在同一個交易；
/// - D3 續簽的重放快取：[`EnrollStore::replay_seen`] 之後 [`EnrollStore::replay_insert`]，同一個交易；
/// - D4 簽發的冪等：交易內以 [`EnrollStore::get_request`] 確認狀態仍是 `approved`，簽章後
///   [`EnrollStore::mark_issued`]。
///
/// PostgreSQL 後端（L1b）改以條件式 UPDATE（`used < max_uses`）、`ON CONFLICT DO NOTHING RETURNING` 與
/// 「先搶占 `issuing`」實作相同的語意，呼叫端不變
pub trait EnrollStore {
    fn insert_token(&self, t: &TokenRow) -> StoreResult<()>;

    fn token_by_hash(&self, hash: &[u8]) -> StoreResult<Option<TokenRow>>;

    fn get_token(&self, token_id: &str) -> StoreResult<Option<TokenRow>>;

    /// 所有 token（新到舊）
    fn list_tokens(&self) -> StoreResult<Vec<TokenRow>>;

    /// 停用（冪等）：回傳停用後的列，不存在時 `None`；`already` 為先前已停用
    fn disable_token(
        &self,
        token_id: &str,
        by: &str,
        reason: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<Option<(TokenRow, bool)>>;

    /// 使用次數 +1（建立申請時，在同一個交易中）
    fn consume_token(&self, token_id: &str, now_ms: i64) -> StoreResult<()>;

    /// 目前是否有有效 token（未停用、未過期、未用完），以及最後一個 token 失效的時間（自動暫停的計時起點）
    fn token_validity(&self, now_ms: i64) -> StoreResult<(usize, Option<i64>)>;

    /// 以批次部署模式生效中的 token 的限定網段（`enroll-rate-limit.md` §4.3）
    fn batch_token_cidrs(&self, now_ms: i64) -> StoreResult<Vec<String>>;

    fn insert_request(&self, q: &RequestRow) -> StoreResult<()>;

    fn get_request(&self, request_id: &str) -> StoreResult<Option<RequestRow>>;

    /// 冪等鍵：同一 `agent_id`＋同一 CSR
    fn request_by_csr(&self, agent_id: &str, csr_sha256: &[u8]) -> StoreResult<Option<RequestRow>>;

    /// 申請編號（不含連字號、大寫）對到的申請（不保證唯一）
    fn requests_by_code(&self, code: &str) -> StoreResult<Vec<RequestRow>>;

    fn list_requests(&self, f: &RequestFilter) -> StoreResult<Vec<RequestRow>>;

    /// 待核准佇列的筆數（L9）
    fn pending_count(&self) -> StoreResult<i64>;

    /// 同一來源的待核准筆數（動態 `poll_after_ms`）
    fn pending_count_from(&self, source_ip: &str) -> StoreResult<i64>;

    /// 最近 `window_ms` 內同一來源建立的申請數（異常旗標 `burst_from_ip`）
    fn recent_count_from(&self, source_ip: &str, since_ms: i64) -> StoreResult<i64>;

    /// 同一 `agent_id` 其他待核准的新註冊申請改為 `replaced`（回傳被取代的 request_id）
    fn replace_pending_for_agent(
        &self,
        agent_id: &str,
        keep_request_id: &str,
        now_ms: i64,
    ) -> StoreResult<Vec<String>>;

    /// 其他主機名稱相同、待核准或已簽發的申請是否存在（異常旗標 `duplicate_hostname`）
    fn hostname_in_use(&self, hostname: &str, agent_id: &str) -> StoreResult<bool>;

    /// 更新 `poll_secret_hash`（過期續簽的冪等重送：待核准時以這次的值為準）
    fn update_poll_secret(&self, request_id: &str, hash: &[u8]) -> StoreResult<()>;

    /// 核准（不簽發）：`pending_approval` → `approved`。回傳是否有變更（狀態已不同時 `false`）
    fn approve(
        &self,
        request_id: &str,
        by: &str,
        lifetime_days: Option<i64>,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 拒絕：待核准 → `rejected`
    fn reject(
        &self,
        request_id: &str,
        by: &str,
        code: &str,
        reason: &str,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 簽發完成：`issued`（決定者沿用核准時的值；自動核准時寫入 `decided_by`）
    fn mark_issued(
        &self,
        request_id: &str,
        serial: &str,
        decided_by: &str,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 簽發失敗的原因（主控台顯示；申請維持 `approved`）
    fn set_issue_error(&self, request_id: &str, error: &str) -> StoreResult<()>;

    /// 撤回：待核准 → `withdrawn`
    fn withdraw(
        &self,
        request_id: &str,
        reason: &str,
        initiated_by: &str,
        ip: &str,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 第一次在輪詢回應中交出憑證
    fn mark_delivered(&self, request_id: &str, now_ms: i64) -> StoreResult<()>;

    /// 新增異常旗標（不重複）
    fn add_flag(&self, request_id: &str, flag: &str) -> StoreResult<()>;

    /// 已核准、尚未簽發的申請（常駐行程每 2 秒簽發；離線模式的匯出）
    fn approved_requests(&self, limit: usize) -> StoreResult<Vec<RequestRow>>;

    /// 保留清除：待核准逾期改為 `request_expired`；結束超過 `closed_keep_ms` 的刪除。回傳 (逾期, 刪除)
    fn expire_and_purge(&self, now_ms: i64, closed_keep_ms: i64) -> StoreResult<(usize, usize)>;

    /// 重放快取中是否有未過期的鍵
    fn replay_seen(&self, key: &[u8], now_ms: i64) -> StoreResult<bool>;

    /// 寫入重放快取（鍵已存在但過期時覆寫）
    fn replay_insert(
        &self,
        key: &[u8],
        agent_id: &str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> StoreResult<()>;

    /// 未過期的重放快取筆數（上限 100,000）
    fn replay_count(&self, now_ms: i64) -> StoreResult<i64>;

    fn replay_purge(&self, now_ms: i64) -> StoreResult<usize>;

    fn load_blocks(&self) -> StoreResult<Vec<BlockRow>>;

    fn upsert_block(&self, b: &BlockRow) -> StoreResult<()>;

    /// 封鎖到期、且 24 小時沒有再犯的列刪除
    fn purge_blocks(&self, now_ms: i64) -> StoreResult<usize>;

    /// 生效中的暫時放寬（CIDR）
    fn active_relaxations(&self, now_ms: i64) -> StoreResult<Vec<String>>;

    fn purge_relaxations(&self, now_ms: i64) -> StoreResult<usize>;

    /// 最近的稽核事件（測試與 CLI）：(時間, 執行者, 動作, 對象, 結果, 代碼)
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    fn recent_audit(
        &self,
        limit: usize,
    ) -> StoreResult<Vec<(i64, String, String, Option<String>, String, Option<String>)>>;

    fn host_enroll(&self, agent_id: &str) -> StoreResult<Option<HostEnroll>>;

    /// 簽發時建立或更新清冊：`current_serial`、生命週期回到 `active`；新主機 `source = 'enroll'`、
    /// `first_seen`／`last_seen` ＝ 簽發時間、離線（主控台判為從未連線）
    #[allow(clippy::too_many_arguments)]
    fn upsert_host_on_issue(
        &self,
        agent_id: &str,
        hostname: &str,
        host_fingerprint: Option<&str>,
        display_name: Option<&str>,
        tags: Option<&str>,
        os: Option<&str>,
        serial: &str,
        now_ms: i64,
    ) -> StoreResult<()>;

    /// 主機群組（角色指派的範圍）：還沒有群組的主機，採用簽發它的註冊 token 的群組
    fn set_host_group_from_token(&self, agent_id: &str, token_id: Option<&str>) -> StoreResult<()>;

    /// 除役（生命週期 → `decommissioned`）；清冊沒有這台時建立最小的紀錄。回傳是否先前已除役
    fn decommission_host(
        &self,
        agent_id: &str,
        hostname: &str,
        by: &str,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 「允許再次自動續簽」（S14，D12）
    fn set_grace_override(&self, agent_id: &str, until_ms: Option<i64>) -> StoreResult<bool>;

    /// 過期續簽的最後一次嘗試（S14「最後嘗試」）
    fn set_last_grace_attempt(&self, agent_id: &str, attempt: &Value) -> StoreResult<()>;

    /// 已除役的主機（`hosts.lifecycle = 'decommissioned'`）的 agent_id
    fn decommissioned_agents(&self) -> StoreResult<std::collections::HashSet<String>>;

    /// 刪除停用或到期早於 `before_ms` 的註冊 token；回傳刪除的筆數
    fn purge_old_tokens(&self, before_ms: i64) -> StoreResult<usize>;

    /// 影響數字的來源（`enroll::ops::impact`）：有 Kairos 簽發的最新憑證、未撤銷的啟用中主機的
    /// (憑證到期, 寬限期延長到, 是否線上, 最後出現)
    fn impact_hosts(&self) -> StoreResult<Vec<(i64, Option<i64>, bool, i64)>>;

    /// 待核准與已核准待簽發的新註冊申請數
    fn pending_enroll_count(&self) -> StoreResult<i64>;

    /// 憑證已過期（`not_after <= now_ms`）、未撤銷的啟用中主機：(agent_id, 名稱, 憑證到期, 寬限期延長到)
    fn expired_cert_hosts(
        &self,
        now_ms: i64,
    ) -> StoreResult<Vec<(String, String, i64, Option<i64>)>>;

    /// token 的部署進度：各狀態的 (狀態, 筆數, 簽發後已連線的主機數)
    fn token_progress_counts(&self, token_id: &str)
    -> StoreResult<Vec<(String, i64, Option<i64>)>>;

    /// 已核准超過 `before_ms` 仍未簽發的申請：(筆數, 最早的核准時間)
    fn stale_approvals(&self, before_ms: i64) -> StoreResult<(i64, Option<i64>)>;
}
