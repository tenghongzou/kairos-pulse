//! 雙人覆核政策（`[remote] rollout_approval`）的前值與改變的紀錄（ADR-0002 決策點 2i、切片 S3；
//! REMOTE.md 第 5.4 節）。
//!
//! `rollout_approval` 只能在設定檔（或環境變數）修改，需要重新啟動。能修改設定檔的人可以獨自把它由
//! `required` 改為 `none`——這是**已接受的治理缺口**；Kairos 能做的是讓這件事無法安靜地發生：
//!
//! - 前值保存在資料庫（`meta` 表的 [`META_KEY`]，JSON；隨備份、`kairos db migrate` 與叢集共用）；
//! - 啟動時（[`RemoteService::observe_approval_policy`]，叢集中只有取得租約的節點會啟動服務）與目前的值
//!   比對，改變時寫稽核 `settings.rollout_approval.change`；第一次記錄時寫 `…baseline`；
//! - **比對與稽核在遠端管理開始接受請求之前、在同一個交易中寫入；寫入失敗時遠端管理不啟動**（主控台回 503
//!   `remote_unavailable`，其餘功能照常）；
//! - `required` → `none` 另開系統規則「雙人覆核已關閉」的告警事件（警告等級、預設推播；
//!   [`RemoteService::reconcile_alerts`]），到改回 `required` 為止不會自己結束。
//!
//! [`RemoteService::observe_approval_policy`]: super::RemoteService::observe_approval_policy
//! [`RemoteService::reconcile_alerts`]: super::RemoteService::reconcile_alerts

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ops::{Actor, TxError};
use super::rollout::ROLLOUT_PENDING_APPROVAL;
use crate::config::{ROLLOUT_APPROVAL_NONE, ROLLOUT_APPROVAL_REQUIRED};
use crate::store::db::Store;

/// `meta` 表的鍵：上一次啟動時觀察到的 `rollout_approval`（[`Observed`] 的 JSON）
pub const META_KEY: &str = "remote.rollout_approval_observed";
/// 稽核：第一次記錄（沒有前值）
pub const ACTION_BASELINE: &str = "settings.rollout_approval.baseline";
/// 稽核：與資料庫記錄的上一次設定不同（值或來源）
pub const ACTION_CHANGE: &str = "settings.rollout_approval.change";
/// S20 與 S26 顯示「雙人覆核已在…關閉」的期間
pub const RECENT_CHANGE_MS: i64 = 30 * 86_400_000;

/// 資料庫中的前值
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed {
    /// `none` 或 `required`
    pub value: String,
    /// `file`、`environment` 或 `default`
    pub source: String,
    /// 第一次觀察到這個值與來源的時間（Unix 毫秒）
    pub observed_at: i64,
    /// 觀察到它的節點（叢集的 `node_id`；單機為主機名稱）
    pub node: String,
    /// 值最近一次改變被觀察到的時間與改變前的值（沒有改變過時為 `None`）
    #[serde(default)]
    pub changed_at: Option<i64>,
    #[serde(default)]
    pub changed_from: Option<String>,
    /// 目前是 `none`、而且是從 `required` 關閉的：觀察到關閉的時間。改回 `required` 時清除
    #[serde(default)]
    pub disabled_at: Option<i64>,
}

impl Observed {
    /// API 的表示（清單的 `meta.rollout_approval_change`、系統健康的 `rollout_approval_change`）：最近
    /// [`RECENT_CHANGE_MS`] 內改變過時才有
    pub fn recent_change_json(&self, now_ms: i64) -> Value {
        match self.changed_at {
            Some(at) if now_ms - at < RECENT_CHANGE_MS => json!({
                "from": self.changed_from,
                "to": self.value,
                "at": crate::api::http::ts_ms(at),
                "source": self.source,
                "node": self.node,
            }),
            _ => Value::Null,
        }
    }

    /// 完整的紀錄（系統健康的 `rollout_approval_observed`、S21 的「上一次改變」）
    pub fn json(&self) -> Value {
        json!({
            "value": self.value,
            "source": self.source,
            "observed_at": crate::api::http::ts_ms(self.observed_at),
            "node": self.node,
            "changed_at": self.changed_at.map(crate::api::http::ts_ms),
            "changed_from": self.changed_from,
            "disabled_at": self.disabled_at.map(crate::api::http::ts_ms),
        })
    }
}

/// 比對的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 沒有前值：記下目前的值作為基準
    Baseline,
    /// 沒有前值、目前是 `none`，但資料庫裡有送審或核准過的推送：視同 `required` → `none`
    BaselineDisabled,
    /// 與前值相同
    Unchanged,
    /// `none` → `required`
    Enabled,
    /// `required` → `none`
    Disabled,
    /// 值相同，來源不同（例如由設定檔改為環境變數）
    SourceChanged,
}

/// 啟動時的比對（在一個寫入交易中呼叫）：讀前值、判斷、寫稽核與新的紀錄。稽核的執行者是 `system`
///
/// | 情況 | 處理 |
/// |---|---|
/// | 沒有前值（升級後第一次啟動、全新安裝、換了新的資料庫） | 記下基準，稽核 `…baseline`。目前是 `none` 而資料庫有待核准或核准過的推送時，視同 `required` → `none`（稽核 `…change`，`details.basis = "reviewed_rollouts"`） |
/// | 值與來源都相同 | 不寫任何東西 |
/// | `none` → `required` | 稽核 `…change` |
/// | `required` → `none` | 稽核 `…change`；記下 `disabled_at`（告警事件由 [`super::RemoteService::reconcile_alerts`] 開啟） |
/// | 只有來源改變 | 稽核 `…change`，`details.source_changed = true` |
/// | 還原資料庫備份 | 前值是備份當時的值，與目前不同時照上列處理（可能把還原前就做過的修改再報一次） |
/// | 前值無法解析 | 視同沒有前值，稽核的 `details.previous_unreadable = true` |
pub fn observe(
    s: &dyn Store,
    value: &str,
    source: &str,
    node: &str,
    now_ms: i64,
) -> Result<(Outcome, Observed), TxError> {
    let raw = s.get_meta(META_KEY)?;
    let previous: Option<Observed> = raw.as_deref().and_then(|t| serde_json::from_str(t).ok());
    let unreadable = raw.is_some() && previous.is_none();
    let fresh = |changed_at, changed_from, disabled_at| Observed {
        value: value.to_string(),
        source: source.to_string(),
        observed_at: now_ms,
        node: node.to_string(),
        changed_at,
        changed_from,
        disabled_at,
    };
    let event = |action: &'static str, details: Value| {
        Actor::system().event(
            "settings",
            action,
            Some("remote.rollout_approval".to_string()),
            None,
            details,
        )
    };
    let (outcome, record, audit) = match previous {
        None => {
            // 曾經啟用過雙人覆核的跡象：待核准的推送，或有核准紀錄的推送
            let reviewed = value == ROLLOUT_APPROVAL_NONE
                && s.list_rollouts(None)?
                    .iter()
                    .any(|r| r.approved_at.is_some() || r.state == ROLLOUT_PENDING_APPROVAL);
            if reviewed {
                (
                    Outcome::BaselineDisabled,
                    fresh(
                        Some(now_ms),
                        Some(ROLLOUT_APPROVAL_REQUIRED.to_string()),
                        Some(now_ms),
                    ),
                    Some(event(
                        ACTION_CHANGE,
                        json!({"from": ROLLOUT_APPROVAL_REQUIRED, "to": value, "source": source, "node": node, "basis": "reviewed_rollouts", "previous_unreadable": unreadable}),
                    )),
                )
            } else {
                (
                    Outcome::Baseline,
                    fresh(None, None, None),
                    Some(event(
                        ACTION_BASELINE,
                        json!({"value": value, "source": source, "node": node, "previous_unreadable": unreadable}),
                    )),
                )
            }
        }
        Some(p) if p.value == value && p.source == source => (Outcome::Unchanged, p, None),
        Some(p) => {
            let details = |source_changed: bool| {
                json!({
                    "from": p.value,
                    "to": value,
                    "source": source,
                    "previous_source": p.source,
                    "source_changed": source_changed,
                    "node": node,
                    "previous_node": p.node,
                    "previous_observed_at": crate::api::http::ts_ms(p.observed_at),
                    "basis": "database_record",
                })
            };
            if p.value == value {
                (
                    Outcome::SourceChanged,
                    fresh(p.changed_at, p.changed_from.clone(), p.disabled_at),
                    Some(event(ACTION_CHANGE, details(true))),
                )
            } else if value == ROLLOUT_APPROVAL_NONE {
                (
                    Outcome::Disabled,
                    fresh(Some(now_ms), Some(p.value.clone()), Some(now_ms)),
                    Some(event(ACTION_CHANGE, details(p.source != source))),
                )
            } else {
                (
                    Outcome::Enabled,
                    fresh(Some(now_ms), Some(p.value.clone()), None),
                    Some(event(ACTION_CHANGE, details(p.source != source))),
                )
            }
        }
    };
    if let Some(e) = audit {
        e.append(s, now_ms)?;
        let text = serde_json::to_string(&record)
            .map_err(|e| TxError::Db(crate::enroll::db::DbError(e.to_string())))?;
        s.put_meta(META_KEY, &text)?;
    }
    Ok((outcome, record))
}
