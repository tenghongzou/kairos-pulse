//! 告警的資料表（schema v5，`store/sqlite.rs` 的 `SCHEMA_V5`）：規則、事件、處理紀錄、靜音、通知管道、
//! 發送紀錄、CLI 的動作佇列，以及保留期限與筆數上限的清除。所有函式都是同步的，由呼叫端在 blocking
//! 執行緒上執行（`EnrollDb`）。

use super::engine::{NewDelivery, Timeline};
use super::model::{Channel, Event, Rule, Silence};
use super::secrets::SecretBox;
use crate::store::db::StoreResult;
use serde_json::{Value, json};
use std::collections::HashMap;

/// `meta` 的鍵：告警設定（規則、靜音、管道）的版本；CLI 與 API 修改時加一，執行中的引擎依此重新載入
pub const META_CONFIG_REV: &str = "alerts_config_rev";

/// `meta` 的鍵：引擎的心跳（JSON：`at`、`clean`）；啟動時判斷停機期間與是否正常關機
pub const META_HEARTBEAT: &str = "alerts_heartbeat";

/// 列表的篩選（S06）
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// `active`（觸發中＋已確認）、`needs_me`、`recovered`（已恢復待結案）、`all`
    pub view: String,
    pub severity: Vec<String>,
    pub state: Vec<String>,
    pub rule: Vec<String>,
    pub category: Vec<String>,
    pub agent_id: Option<String>,
    pub q: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// 包含子事件（預設只列頂層）
    pub include_children: bool,
    pub limit: usize,
    /// 游標：(sev_rank, acked, started_at, event_id)
    pub after: Option<(i64, i64, i64, String)>,
}

// ---------------------------------------------------------------------------
// 通知管道
// ---------------------------------------------------------------------------

/// 管道的狀態：機密無法解開（`alerts-secrets.key` 遺失、與資料庫不成對，或密文被修改）。
/// 不會嘗試發送，視同失敗（[`Channel::failed`]），由「通知管道失敗」系統規則以 `KRS-E5110` 開事件
/// （設計包 `decisions/INTEGRATION.md` §28.5 #8、`alerts-ux.md` 5.5）
pub const STATE_SECRETS_UNAVAILABLE: &str = "secrets_unavailable";

/// [`STATE_SECRETS_UNAVAILABLE`] 時的錯誤訊息（API、CLI 與發送紀錄）
pub const SECRETS_UNAVAILABLE_MESSAGE: &str = "無法解開管道的機密：資料目錄的 alerts-secrets.key 遺失或與資料庫不成對。請從備份還原金鑰檔（kairos db restore-keys）並重新啟動，或重新輸入這個管道的機密";

// ---------------------------------------------------------------------------
// 發送紀錄
// ---------------------------------------------------------------------------

/// 一筆發送紀錄
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub id: i64,
    pub event_id: Option<String>,
    pub channel_id: String,
    pub kind: String,
    pub state: String,
    pub urgent: bool,
    pub payload: Value,
    pub attempts: u32,
    pub created_at: i64,
    pub next_attempt_at: i64,
    pub sent_at: Option<i64>,
    pub batch_id: Option<i64>,
    pub response_code: Option<i64>,
    pub error_code: Option<String>,
    pub error: Option<String>,
}

/// 發送紀錄的 JSON（不含內容：內容可以從事件重建，且可能含有主機的 IP）
pub fn delivery_json(d: &Delivery, channel_name: Option<&str>) -> Value {
    let t = crate::api::http::ts_ms;
    json!({
        "id": d.id,
        "event_id": d.event_id,
        "channel_id": d.channel_id,
        "channel_name": channel_name,
        "kind": d.kind,
        "state": d.state,
        "attempts": d.attempts,
        "created_at": t(d.created_at),
        "next_attempt_at": (d.state == "pending" || d.state == "held").then(|| t(d.next_attempt_at)),
        "sent_at": d.sent_at.map(t),
        "response_code": d.response_code,
        "error_code": d.error_code,
        "error": d.error,
    })
}

// ---------------------------------------------------------------------------
// CLI 的動作佇列
// ---------------------------------------------------------------------------

/// 一個待處理的動作（`kairos alerts ack` 等由執行中的 Kairos 處理）
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub id: i64,
    pub created_at: i64,
    pub actor: String,
    pub event_id: String,
    pub action: String,
    pub params: Value,
}

// ---------------------------------------------------------------------------
// 保留期限與筆數上限
// ---------------------------------------------------------------------------

/// 清除的結果
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Purged {
    pub events: u64,
    pub timeline: u64,
    pub deliveries: u64,
    pub silences: u64,
    pub actions: u64,
}

/// 告警的儲存：規則、事件與時間軸、靜音、通知管道、發送紀錄、動作佇列，以及 `meta` 中的設定版本。
/// SQLite 的實作在 `store/sqlite/alerts.rs`
///
/// `kairos/HA.md` 第 5.4 節：
///
/// - D6 設定版本：[`AlertStore::bump_config_rev`] 先讀再加一，必須在寫入規則、靜音或管道的同一個交易中
///   （SQLite 以寫入鎖序列化；PostgreSQL 後端改為 `cluster_revs` 的原子遞增）；
/// - D7 事件流水號：告警引擎啟動時以 [`AlertStore::load_seq`] 讀出各日的最大流水號，之後在記憶體中遞增
///   （只有一個引擎寫入；PostgreSQL 後端另以 `alert_event_seq` 防止換手時撞號）；
/// - 事件、時間軸與發送紀錄的寫入（[`AlertStore::upsert_events`]、[`AlertStore::insert_timeline`]、
///   [`AlertStore::insert_deliveries`]）必須在交易中呼叫
pub trait AlertStore {
    /// 告警判定用的主機清冊（每 30 秒讀一次）
    fn alert_inventory(&self) -> StoreResult<Vec<super::InvHost>>;

    /// 重新啟動前「在線上」的主機：清冊的 `last_seen` 不早於 `since_ms` 的啟用中主機
    fn active_hosts_seen_since(
        &self,
        since_ms: i64,
    ) -> StoreResult<std::collections::HashSet<String>>;

    /// 設定的版本（不存在時 0）
    fn config_rev(&self) -> StoreResult<i64>;

    /// 設定已修改：版本加一
    fn bump_config_rev(&self) -> StoreResult<()>;

    /// 補齊系統預設規則（已存在的不覆寫）；回傳新增的筆數
    fn ensure_builtin_rules(&self, now_ms: i64) -> StoreResult<usize>;

    fn load_rules(&self) -> StoreResult<Vec<Rule>>;

    fn get_rule(&self, id: &str) -> StoreResult<Option<Rule>>;

    /// 新增或整列取代
    fn save_rule(&self, r: &Rule) -> StoreResult<()>;

    fn delete_rule(&self, id: &str) -> StoreResult<bool>;

    /// 每條規則最近 `days` 天的觸發次數與「雜訊」（10 分鐘內自行恢復且未被確認的比例；S08）
    fn rule_stats(&self, since_ms: i64) -> StoreResult<HashMap<String, (u64, u64)>>;

    /// 寫回事件（整列）
    fn upsert_events(&self, events: &[Event]) -> StoreResult<()>;

    fn insert_timeline(&self, items: &[Timeline]) -> StoreResult<()>;

    /// 排入通知；回傳每筆的 id（與輸入同順序）
    fn insert_deliveries(&self, items: &[NewDelivery]) -> StoreResult<Vec<i64>>;

    /// 尚未解決的事件
    fn load_open_events(&self) -> StoreResult<Vec<Event>>;

    fn get_event(&self, id: &str) -> StoreResult<Option<Event>>;

    /// 手動解決時條件仍成立的對象（最近 30 天）：去重鍵 → 異常的開始時間
    fn load_suppressed(&self, since_ms: i64) -> StoreResult<HashMap<String, i64>>;

    /// 事件編號已用到的流水號（最近幾天）
    fn load_seq(&self, days: &[String]) -> StoreResult<HashMap<String, u64>>;

    /// 處理紀錄（依時間）
    fn timeline(&self, event_id: &str, limit: usize) -> StoreResult<Vec<Value>>;

    /// 子事件（最多 `limit` 筆）與總數
    fn children(&self, parent: &str, limit: usize) -> StoreResult<(Vec<Event>, u64)>;

    /// 列表：預設排序（嚴重度 → 未確認優先 → 開始時間由新到舊）與總數
    fn list_events(&self, f: &EventFilter) -> StoreResult<(Vec<Event>, u64)>;

    /// 各檢視的數量（S06 頁籤、SideNav 徽章；只含需要處理、頂層、未靜音）
    fn counts(&self) -> StoreResult<Value>;

    /// 結束時間在 `since_ms` 之後的靜音（引擎：進行中、即將開始與剛結束的）
    fn load_silences(&self, since_ms: i64) -> StoreResult<Vec<Silence>>;

    /// 列表（`status`：`active`、`upcoming`、`ended`、`all`）
    fn list_silences(&self, status: &str, now_ms: i64, limit: usize) -> StoreResult<Vec<Silence>>;

    fn get_silence(&self, id: &str) -> StoreResult<Option<Silence>>;

    fn save_silence(&self, s: &Silence) -> StoreResult<()>;

    fn mark_silences_notified(&self, ids: &[String]) -> StoreResult<()>;

    /// 所有管道；`secrets` 為 `None` 時不解密（列表與 API 只需要遮蔽值）
    fn load_channels(&self, secrets: Option<&SecretBox>) -> StoreResult<Vec<Channel>>;

    fn get_channel(&self, id: &str, secrets: Option<&SecretBox>) -> StoreResult<Option<Channel>>;

    /// 新增或更新管道的設定與機密（`sealed` 為 `None` 時保留原本的機密）
    fn save_channel(&self, c: &Channel, sealed: Option<Vec<u8>>) -> StoreResult<()>;

    fn delete_channel(&self, id: &str) -> StoreResult<bool>;

    /// 派送的結果寫回管道的狀態（不改版本：引擎在下一個 tick 以派送端的狀態為準）
    fn update_channel_status(&self, c: &Channel) -> StoreResult<()>;

    /// 使用這個管道的規則（刪除管道時列出受影響的規則，S11 L2）
    fn rules_using_channel(&self, id: &str) -> StoreResult<Vec<(String, String)>>;

    /// 待送的通知（含暫緩的）
    fn pending_deliveries(&self) -> StoreResult<Vec<Delivery>>;

    /// 發送的結果
    fn update_delivery(&self, d: &Delivery) -> StoreResult<()>;

    /// 一個事件的發送紀錄（S07「通知發送狀態」）
    fn deliveries_of_event(&self, event_id: &str, limit: usize) -> StoreResult<Vec<Delivery>>;

    /// 一個管道最近的發送紀錄
    fn deliveries_of_channel(&self, channel_id: &str, limit: usize) -> StoreResult<Vec<Delivery>>;

    fn enqueue_action(
        &self,
        now_ms: i64,
        actor: &str,
        event_id: &str,
        action: &str,
        p: &Value,
    ) -> StoreResult<i64>;

    fn pending_actions(&self) -> StoreResult<Vec<Action>>;

    fn finish_action(&self, id: i64, now_ms: i64, result: &Value) -> StoreResult<()>;

    /// 動作的結果（CLI 等待用）：`None` 為尚未處理
    fn action_result(&self, id: i64) -> StoreResult<Option<Value>>;

    /// 依保留天數與筆數上限刪除已解決的事件（最多 `batch` 筆；回傳是否還有要刪的）
    fn purge_alerts(
        &self,
        now_ms: i64,
        retention_days: u32,
        max_events: u64,
        batch: i64,
    ) -> StoreResult<(Purged, bool)>;
}
