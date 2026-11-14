//! 升級推送（REMOTE.md 第 4.7 節；0.10 起：自訂批次（最多 [`MAX_WAVES`] 批）、排程與升級時段
//! （[`super::schedule`]）；雙人覆核另見第 5.4 節）。
//!
//! - [`RolloutSpec`]：建立推送的內容（名稱、目標版本、選擇器、批次、選項）；JSON 存在 `rollouts`。
//! - [`classify`]：資格分類（第 4.7.2 節），送出（submit）時固定推送的對象並寫入 `rollout_hosts`。
//! - 推送引擎（[`RemoteService::tick`](super::RemoteService)，`remote/mod.rs`）：逾時、提議新主機、
//!   批次的觀察期與失敗比例、自動暫停。這裡只有純函式（[`plan_tick`]），方便以假的時鐘測試。
//!
//! 主機的狀態：`pending` → `offered` → `accepted` → `staged` → `applying` → `succeeded`／`failed`／
//! `rolled_back`／`rollback_failed`；另有 `declined`、`expired`、`skipped`（`reason`）、`skipped_offline`、
//! `unknown`（替換後超過期限沒有回報且離線）、`cancelled`（推送取消時尚未開始替換）。

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

use kairos_proto::mgmt::{
    DEFAULT_HEALTH_TIMEOUT_SECS, INSTALL_EXTERNAL_URL_ALLOWLIST, INSTALL_EXTERNAL_URL_NONE,
    INSTALL_METHOD_DEB, INSTALL_METHOD_RPM, INSTALL_UPGRADE_READY, INSTALL_UPGRADE_UNSAFE_PATH,
    MAX_HEALTH_TIMEOUT_SECS, MIN_HEALTH_TIMEOUT_SECS,
};
use kairos_proto::release::{self, Os, ReleaseManifest, Version};

use super::schedule::{ScheduleSpec, WindowState};
use super::store::{ArtifactRow, RolloutHostRow, UpgradeHost};

// ---- 推送的狀態 ----

pub const ROLLOUT_DRAFT: &str = "draft";
/// 已送出、等待另一位有 `rollouts:approve` 的使用者核准（雙人覆核，REMOTE.md 第 5.4 節；0.10）
pub const ROLLOUT_PENDING_APPROVAL: &str = "pending_approval";
/// 已送出、等待 `schedule.starts_at`（0.10）
pub const ROLLOUT_SCHEDULED: &str = "scheduled";
pub const ROLLOUT_RUNNING: &str = "running";
pub const ROLLOUT_PAUSED: &str = "paused";
pub const ROLLOUT_COMPLETED: &str = "completed";
pub const ROLLOUT_CANCELLED: &str = "cancelled";

// ---- 主機的狀態 ----

pub const HOST_PENDING: &str = "pending";
pub const HOST_OFFERED: &str = "offered";
pub const HOST_ACCEPTED: &str = "accepted";
pub const HOST_STAGED: &str = "staged";
pub const HOST_APPLYING: &str = "applying";
pub const HOST_SUCCEEDED: &str = "succeeded";
pub const HOST_FAILED: &str = "failed";
pub const HOST_ROLLED_BACK: &str = "rolled_back";
pub const HOST_ROLLBACK_FAILED: &str = "rollback_failed";
pub const HOST_DECLINED: &str = "declined";
pub const HOST_EXPIRED: &str = "expired";
pub const HOST_SKIPPED: &str = "skipped";
pub const HOST_SKIPPED_OFFLINE: &str = "skipped_offline";
pub const HOST_UNKNOWN: &str = "unknown";
pub const HOST_CANCELLED: &str = "cancelled";

/// 所有主機狀態（API 的篩選、計數的鍵）
pub const HOST_STATES: [&str; 15] = [
    HOST_PENDING,
    HOST_OFFERED,
    HOST_ACCEPTED,
    HOST_STAGED,
    HOST_APPLYING,
    HOST_SUCCEEDED,
    HOST_FAILED,
    HOST_ROLLED_BACK,
    HOST_ROLLBACK_FAILED,
    HOST_DECLINED,
    HOST_EXPIRED,
    HOST_SKIPPED,
    HOST_SKIPPED_OFFLINE,
    HOST_UNKNOWN,
    HOST_CANCELLED,
];

/// 提議中、尚未替換（暫停或取消時回到 `pending` 或 `cancelled`；Pulse 收到 `upgrade_unavailable`）
pub fn is_offered(state: &str) -> bool {
    matches!(state, HOST_OFFERED | HOST_ACCEPTED | HOST_STAGED)
}

/// 在下載到確認之間（計入 `max_in_progress`）
pub fn is_in_progress(state: &str) -> bool {
    is_offered(state) || state == HOST_APPLYING
}

/// 這台主機在這次推送中已經結束（批次可以結束）
pub fn is_terminal(state: &str) -> bool {
    matches!(
        state,
        HOST_SUCCEEDED
            | HOST_FAILED
            | HOST_ROLLED_BACK
            | HOST_ROLLBACK_FAILED
            | HOST_DECLINED
            | HOST_EXPIRED
            | HOST_SKIPPED
            | HOST_SKIPPED_OFFLINE
            | HOST_UNKNOWN
            | HOST_CANCELLED
    )
}

/// 計入失敗比例的分子
pub fn is_failure(state: &str) -> bool {
    matches!(
        state,
        HOST_FAILED | HOST_ROLLED_BACK | HOST_ROLLBACK_FAILED | HOST_UNKNOWN
    )
}

/// 計入失敗比例的分母（`declined`、`skipped`、`expired` 不計）
fn counts_for_ratio(state: &str) -> bool {
    state == HOST_SUCCEEDED || is_failure(state)
}

/// 可以重試（新的 `upgrade_id`）
pub fn is_retryable(h: &RolloutHostRow) -> bool {
    matches!(
        h.state.as_str(),
        HOST_FAILED
            | HOST_ROLLED_BACK
            | HOST_EXPIRED
            | HOST_SKIPPED_OFFLINE
            | HOST_DECLINED
            | HOST_UNKNOWN
    ) || (h.state == HOST_ROLLBACK_FAILED && h.acknowledged_at.is_some())
}

// ---- 預設值與上限 ----

/// `max_in_progress` 的預設與範圍
pub const DEFAULT_MAX_IN_PROGRESS: u32 = 50;
pub const MAX_IN_PROGRESS: u32 = 1000;
/// 離線多久的主機標為 `skipped_offline`（預設 24 小時）
pub const DEFAULT_OFFLINE_GRACE_HOURS: u32 = 24;
pub const MAX_OFFLINE_GRACE_HOURS: u32 = 720;
/// 觀察期的上限（7 天）
pub const MAX_SOAK_MINUTES: u32 = 10_080;
/// 批次數上限（0.9.0 為 2：canary＋其餘；0.10 起可自訂）
pub const MAX_WAVES: usize = 10;
/// 選擇器每個清單的項目上限
pub const MAX_SELECTOR_ITEMS: usize = 10_000;
/// 推送名稱的長度上限（字元）
pub const MAX_ROLLOUT_NAME_CHARS: usize = 128;
/// 提議的 `apply_window`：提議後 24 小時內開始替換（0.9.0 沒有升級時段）
pub const APPLY_WINDOW_MS: i64 = 24 * 3600 * 1000;
/// 提議後超過 `apply_window` 再加這段時間仍未開始替換：Kairos 端標為 `expired`
pub const EXPIRE_GRACE_MS: i64 = 3600 * 1000;
/// `applying` 之後超過 `health_timeout_secs` 再加這段時間沒有回報且離線：`unknown`
pub const APPLY_STUCK_GRACE_MS: i64 = 30 * 60 * 1000;
/// 升級中主機的離線告警抑制：`applying` 之後 `health_timeout_secs` 再加這段時間（REMOTE.md 第 4.7.5 節）
pub const SUPPRESS_GRACE_MS: i64 = 10 * 60 * 1000;

/// 離線告警抑制的期限：主機進入 `applying` 的時間＋`health_timeout_secs`＋10 分鐘。主機在這之前到達終止
/// 狀態時提早結束；超過後仍離線就照常告警
pub fn suppress_until(applying_at: i64, health_timeout_secs: u32) -> i64 {
    applying_at + i64::from(health_timeout_secs) * 1000 + SUPPRESS_GRACE_MS
}

/// 有升級時段的推送：時段結束後超過這段時間仍未開始替換的主機回到 `pending`，下一個時段重新提議
/// （留給 Pulse 回報 `expired` 的時間）
pub const WINDOW_EXPIRE_GRACE_MS: i64 = 2 * 60 * 1000;
/// 有升級時段的推送：一台主機因時段結束而重新排入的次數上限，超過後留在 `expired`（可手動重試）
pub const MAX_WINDOW_REQUEUES: i64 = 10;
/// `ends_at` 到了而尚未開始替換的主機的 `reason`
pub const REASON_ROLLOUT_ENDED: &str = "rollout_ended";
/// Kairos 啟動後這段時間內不提議新的主機（REMOTE.md 第 4.7.4 節第 5 項）
pub const START_GRACE_MS: i64 = 60 * 1000;

// ---- 推送的內容 ----

/// 選擇器：`all`，或 `groups`、`tags`、`agent_ids` 的聯集，再減去 `exclude_agent_ids`
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SelectorSpec {
    pub all: bool,
    pub groups: Vec<String>,
    pub tags: Vec<String>,
    pub agent_ids: Vec<String>,
    pub exclude_agent_ids: Vec<String>,
}

impl SelectorSpec {
    pub fn matches(&self, h: &UpgradeHost) -> bool {
        if self.exclude_agent_ids.contains(&h.agent_id) {
            return false;
        }
        self.all
            || self.agent_ids.contains(&h.agent_id)
            || h.host_group
                .as_deref()
                .is_some_and(|g| self.groups.contains(&g.trim().to_lowercase()))
            || self.tags.iter().any(|t| h.tags.contains(t))
    }

    fn validate(&mut self) -> Result<(), (String, String)> {
        let bad = |f: &str, m: String| Err((format!("selector.{f}"), m));
        for (field, list) in [
            ("groups", &mut self.groups),
            ("tags", &mut self.tags),
            ("agent_ids", &mut self.agent_ids),
            ("exclude_agent_ids", &mut self.exclude_agent_ids),
        ] {
            if list.len() > MAX_SELECTOR_ITEMS {
                return bad(field, format!("最多 {MAX_SELECTOR_ITEMS} 項"));
            }
            for v in list.iter_mut() {
                *v = v.trim().to_string();
                if v.is_empty() || v.chars().any(kairos_proto::is_forbidden_display_char) {
                    return bad(field, "不可有空字串或控制字元".to_string());
                }
                match field {
                    "groups" => *v = v.to_lowercase(),
                    "agent_ids" | "exclude_agent_ids" => {
                        if !kairos_proto::is_uuid_text(v) {
                            return bad(field, format!("「{v}」不是 agent_id（UUID）"));
                        }
                        *v = v.to_ascii_lowercase();
                    }
                    _ => {}
                }
            }
            list.sort();
            list.dedup();
        }
        if !self.all && self.groups.is_empty() && self.tags.is_empty() && self.agent_ids.is_empty()
        {
            return Err((
                "selector".to_string(),
                "至少要有 all、groups、tags、agent_ids 其中之一".to_string(),
            ));
        }
        Ok(())
    }
}

/// 一批的大小
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveSize {
    /// 台數
    Count(u32),
    /// 可升級主機的百分比（1～100，至少 1 台）
    Percent(u32),
    /// 其餘的主機（最後一批）
    Rest,
}

impl WaveSize {
    fn parse(v: &Value) -> Result<WaveSize, String> {
        match v {
            Value::Number(n) => n
                .as_u64()
                .filter(|n| (1..=1_000_000).contains(n))
                .map(|n| WaveSize::Count(n as u32))
                .ok_or_else(|| "size 必須是 1 以上的整數、\"N%\" 或 \"rest\"".to_string()),
            Value::String(s) if s == "rest" => Ok(WaveSize::Rest),
            Value::String(s) => s
                .strip_suffix('%')
                .and_then(|p| p.trim().parse::<u32>().ok())
                .filter(|p| (1..=100).contains(p))
                .map(WaveSize::Percent)
                .ok_or_else(|| "size 必須是 1 以上的整數、\"N%\"（1～100）或 \"rest\"".to_string()),
            _ => Err("size 必須是 1 以上的整數、\"N%\" 或 \"rest\"".to_string()),
        }
    }

    fn json(self) -> Value {
        match self {
            WaveSize::Count(n) => json!(n),
            WaveSize::Percent(p) => json!(format!("{p}%")),
            WaveSize::Rest => json!("rest"),
        }
    }
}

/// 一批
#[derive(Debug, Clone, PartialEq)]
pub struct WaveSpec {
    pub size: WaveSize,
    /// `Percent` 時的上限台數（預設範本的 canary：1%、至多 10 台）
    pub max_hosts: Option<u32>,
    /// 這批全部結束後的觀察時間
    pub soak_minutes: u32,
    /// 超過即暫停（0～1）
    pub max_failure_ratio: f64,
    /// 觀察期滿後要人工按「繼續」
    pub manual_continue: bool,
}

impl WaveSpec {
    pub fn json(&self) -> Value {
        json!({
            "size": self.size.json(),
            "max_hosts": self.max_hosts,
            "soak_minutes": self.soak_minutes,
            "max_failure_ratio": self.max_failure_ratio,
            "manual_continue": self.manual_continue,
        })
    }

    /// 預設範本（REMOTE.md 第 4.7.1 節收斂為兩批）：canary 1%（至少 1 台、至多 10 台）、觀察 60 分鐘、
    /// 任何失敗都暫停、人工繼續；其餘 5%
    pub fn default_template() -> Vec<WaveSpec> {
        vec![
            WaveSpec {
                size: WaveSize::Percent(1),
                max_hosts: Some(10),
                soak_minutes: 60,
                max_failure_ratio: 0.0,
                manual_continue: true,
            },
            WaveSpec {
                size: WaveSize::Rest,
                max_hosts: None,
                soak_minutes: 0,
                max_failure_ratio: 0.05,
                manual_continue: false,
            },
        ]
    }

    /// REMOTE.md 第 4.7.1 節的四批「建議」範本（主控台選用）：canary 1%（至多 10 台、觀察 60 分鐘、
    /// 任何失敗都暫停、人工繼續）→ 10% → 50%（各觀察 30 分鐘、上限 5%）→ 其餘。百分比以**剩餘**的
    /// 可升級主機計
    pub fn recommended_template() -> Vec<WaveSpec> {
        let mid = |p: u32| WaveSpec {
            size: WaveSize::Percent(p),
            max_hosts: None,
            soak_minutes: 30,
            max_failure_ratio: 0.05,
            manual_continue: false,
        };
        let mut w = WaveSpec::default_template();
        w.splice(1..1, [mid(10), mid(50)]);
        w
    }

    fn parse(v: &Value, last: bool) -> Result<WaveSpec, String> {
        let Value::Object(m) = v else {
            return Err("每一批必須是物件".to_string());
        };
        for k in m.keys() {
            if !matches!(
                k.as_str(),
                "size" | "max_hosts" | "soak_minutes" | "max_failure_ratio" | "manual_continue"
            ) {
                return Err(format!("不認得的欄位 {k}"));
            }
        }
        let size = WaveSize::parse(m.get("size").unwrap_or(&Value::Null))?;
        if last != (size == WaveSize::Rest) {
            return Err(if last {
                "最後一批的 size 必須是 \"rest\"".to_string()
            } else {
                "只有最後一批可以是 \"rest\"".to_string()
            });
        }
        let max_hosts = match m.get("max_hosts") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .filter(|n| (1..=1_000_000).contains(n))
                    .ok_or("max_hosts 必須是 1 以上的整數")? as u32,
            ),
        };
        let soak_minutes = match m.get("soak_minutes") {
            None | Some(Value::Null) => 0,
            Some(v) => v
                .as_u64()
                .filter(|n| *n <= u64::from(MAX_SOAK_MINUTES))
                .ok_or(format!("soak_minutes 必須介於 0..={MAX_SOAK_MINUTES}"))?
                as u32,
        };
        let max_failure_ratio = match m.get("max_failure_ratio") {
            None | Some(Value::Null) => 0.05,
            Some(v) => v
                .as_f64()
                .filter(|r| (0.0..=1.0).contains(r))
                .ok_or("max_failure_ratio 必須介於 0～1")?,
        };
        let manual_continue = match m.get("manual_continue") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            _ => return Err("manual_continue 必須是布林".to_string()),
        };
        Ok(WaveSpec {
            size,
            max_hosts,
            soak_minutes,
            max_failure_ratio,
            manual_continue,
        })
    }
}

/// 推送的選項
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RolloutOptions {
    pub max_in_progress: u32,
    pub health_timeout_secs: u32,
    /// 允許降版（L3 確認；Pulse 本機也要允許，REMOTE.md 第 4.8 節）
    pub allow_downgrade: bool,
    /// 允許沒有回滾檔的 deb／rpm 主機（L2 確認，第 4.4.2 節）
    pub allow_no_rollback: bool,
    /// 指定第一批的主機（沒有指定時第一批依順序取前 N 台）
    pub canary_agent_ids: Vec<String>,
    /// 離線多久的主機標為 `skipped_offline`
    pub offline_grace_hours: u32,
}

impl Default for RolloutOptions {
    fn default() -> Self {
        Self {
            max_in_progress: DEFAULT_MAX_IN_PROGRESS,
            health_timeout_secs: DEFAULT_HEALTH_TIMEOUT_SECS,
            allow_downgrade: false,
            allow_no_rollback: false,
            canary_agent_ids: Vec::new(),
            offline_grace_hours: DEFAULT_OFFLINE_GRACE_HOURS,
        }
    }
}

/// 建立或修改推送的內容
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutSpec {
    pub name: String,
    pub target_version: String,
    pub selector: SelectorSpec,
    pub waves: Vec<WaveSpec>,
    pub options: RolloutOptions,
    /// 排程與升級時段（沒有時立即開始、隨時可以提議）
    pub schedule: ScheduleSpec,
}

/// 欄位錯誤：(欄位, 說明)
pub type FieldError = (String, String);

impl RolloutSpec {
    /// 解析 API 的本文（`POST /api/v1/rollouts`、`/preview`、`PUT`）
    pub fn parse(body: &Value) -> Result<RolloutSpec, FieldError> {
        let Value::Object(m) = body else {
            return Err(("body".into(), "本文必須是 JSON 物件".into()));
        };
        for k in m.keys() {
            match k.as_str() {
                "name" | "target_version" | "selector" | "waves" | "options" | "schedule"
                | "impact_token" => {}
                "windows" | "starts_at" | "ends_at" | "timezone" => {
                    return Err((k.clone(), format!("{k} 請放在 schedule 物件內")));
                }
                _ => return Err((k.clone(), format!("不認得的欄位 {k}"))),
            }
        }
        let name = m
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if name.is_empty()
            || name.chars().count() > MAX_ROLLOUT_NAME_CHARS
            || name.chars().any(kairos_proto::is_forbidden_display_char)
        {
            return Err((
                "name".into(),
                format!("名稱必須是 1～{MAX_ROLLOUT_NAME_CHARS} 個字元、不含控制字元"),
            ));
        }
        let target_version = m
            .get("target_version")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        Version::parse(target_version)
            .map_err(|e| ("target_version".to_string(), format!("版本不合法：{e}")))?;
        let mut selector: SelectorSpec = match m.get("selector") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| ("selector".to_string(), format!("選擇器不合法：{e}")))?,
            None => return Err(("selector".into(), "需要 selector".into())),
        };
        selector.validate()?;
        let waves = match m.get("waves") {
            None | Some(Value::Null) => WaveSpec::default_template(),
            Some(Value::Array(list)) if !list.is_empty() && list.len() <= MAX_WAVES => list
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    WaveSpec::parse(w, i + 1 == list.len()).map_err(|e| (format!("waves[{i}]"), e))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(("waves".into(), format!("waves 必須是 1～{MAX_WAVES} 批")));
            }
        };
        let mut options: RolloutOptions = match m.get("options") {
            None | Some(Value::Null) => RolloutOptions::default(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| ("options".to_string(), format!("選項不合法：{e}")))?,
        };
        if !(1..=MAX_IN_PROGRESS).contains(&options.max_in_progress) {
            return Err((
                "options.max_in_progress".into(),
                format!("必須介於 1..={MAX_IN_PROGRESS}"),
            ));
        }
        if !(MIN_HEALTH_TIMEOUT_SECS..=MAX_HEALTH_TIMEOUT_SECS)
            .contains(&options.health_timeout_secs)
        {
            return Err((
                "options.health_timeout_secs".into(),
                format!("必須介於 {MIN_HEALTH_TIMEOUT_SECS}..={MAX_HEALTH_TIMEOUT_SECS}"),
            ));
        }
        if !(1..=MAX_OFFLINE_GRACE_HOURS).contains(&options.offline_grace_hours) {
            return Err((
                "options.offline_grace_hours".into(),
                format!("必須介於 1..={MAX_OFFLINE_GRACE_HOURS}"),
            ));
        }
        for a in options.canary_agent_ids.iter_mut() {
            if !kairos_proto::is_uuid_text(a.trim()) {
                return Err((
                    "options.canary_agent_ids".into(),
                    format!("「{a}」不是 agent_id（UUID）"),
                ));
            }
            *a = a.trim().to_ascii_lowercase();
        }
        options.canary_agent_ids.dedup();
        if !options.canary_agent_ids.is_empty() && waves.len() < 2 {
            return Err((
                "options.canary_agent_ids".into(),
                "指定 canary 主機時需要兩批".into(),
            ));
        }
        let schedule = ScheduleSpec::parse(m.get("schedule"))?;
        Ok(RolloutSpec {
            name: name.to_string(),
            target_version: target_version.to_string(),
            selector,
            waves,
            options,
            schedule,
        })
    }

    pub fn waves_json(&self) -> Value {
        Value::Array(self.waves.iter().map(WaveSpec::json).collect())
    }
}

/// 資料表中的批次（解析失敗時用預設範本並記錄）
pub fn waves_from_db(text: &str) -> Vec<WaveSpec> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| match v {
            Value::Array(list) if !list.is_empty() => list
                .iter()
                .enumerate()
                .map(|(i, w)| WaveSpec::parse(w, i + 1 == list.len()).ok())
                .collect::<Option<Vec<_>>>(),
            _ => None,
        })
        .unwrap_or_else(|| {
            log::warn!("升級推送：資料表中的批次無法解析，改用預設範本");
            WaveSpec::default_template()
        })
}

pub fn options_from_db(text: &str) -> RolloutOptions {
    serde_json::from_str(text).unwrap_or_default()
}

pub fn selector_from_db(text: &str) -> SelectorSpec {
    serde_json::from_str(text).unwrap_or_default()
}

// ---- 資格分類 ----

/// 目標版本與版本庫（資格分類用）
#[derive(Debug, Clone)]
pub struct ReleaseContext {
    pub target: Version,
    pub manifest: Option<ReleaseManifest>,
    /// 目標版本中已存入版本庫的發佈檔
    pub artifacts: Vec<ArtifactRow>,
    /// 版本 → 已存入的發佈檔（回滾檔：主機目前的版本、同格式同平台）
    pub by_version: HashMap<String, Vec<ArtifactRow>>,
    /// 目標版本中資料庫記為已存入、但檔案不存在或大小不符的發佈檔（ADR-0002 F1；`KRS-W8417`）。不在
    /// `artifacts` 與 `by_version` 中：需要它的主機分類為 [`SKIP_ARTIFACT_MISSING`]，而不是可升級
    pub missing: Vec<ArtifactRow>,
    /// 目標版本是外部 URL 的版本庫時的 `base_url`（提議的 `artifact.source` 為 `url`；REMOTE.md 第 4.1.2
    /// 節）。有值時資格分類另看主機的 `install.external_url`（[`SKIP_URL_NOT_ALLOWED`]）；由 Kairos 代管的
    /// 版本為 `None`
    pub external_base_url: Option<String>,
}

/// 資格分類的原因（目標是外部 URL 的版本時）：Pulse 在 hello 宣告 `install.external_url = "none"`——本機的
/// `upgrade.url_allowlist` 是空的，`source: "url"` 的提議一律 `declined`（`PLS-W8411`、`url_not_allowed`；
/// 名稱與 Pulse 回報的 `detail` 相同）。不事先標出的話，這台主機要到推送中才回報 `declined`
pub const SKIP_URL_NOT_ALLOWED: &str = "url_not_allowed";

/// 資格分類的原因：版本庫記錄這個平台的發佈檔已存入，但檔案不存在或大小不符（重新上傳即可；與「版本庫
/// 沒有這個平台的檔案」的 `no_artifact` 分開）。也是推送中主機等待檔案時的 `reason`、API 的 409 錯誤碼
pub const SKIP_ARTIFACT_MISSING: &str = "artifact_missing";

/// 資格分類的原因：Pulse（0.10.0 起）在 hello 回報 `install.upgrade = "unsafe_path"`——安裝目錄、執行檔、
/// 輔助程式的工作目錄或它們的上層目錄沒有通過升級輔助程式的路徑檢查（REMOTE.md 第 17.9.7 節）。不事先
/// 標出的話，這台主機要到推送時才以 `PLS-E8405`（`unsafe_path`）失敗
pub const SKIP_UNSAFE_PATH: &str = "unsafe_path";

/// 這台主機的發佈檔（依 `install.method` 與 `os.arch`）
pub fn pick_artifact<'a>(
    artifacts: &'a [ArtifactRow],
    install_method: &str,
    arch: &str,
) -> Option<&'a ArtifactRow> {
    let (os, arch, format) = release::artifact_kind(install_method, arch)?;
    artifacts.iter().find(|a| {
        a.stored && a.os == os.as_str() && a.arch == arch.as_str() && a.format == format.extension()
    })
}

/// hello 的 `os`（JSON）中的 `arch`
pub fn arch_of(os_json: Option<&str>) -> Option<String> {
    os_json
        .and_then(|t| serde_json::from_str::<kairos_proto::OsInfo>(t).ok())
        .and_then(|o| o.arch)
}

/// 一台主機的資格
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eligibility {
    /// `None`：可升級；`Some(原因)`：跳過
    pub skip: Option<&'static str>,
    pub from_version: Option<String>,
    /// 降版
    pub downgrade: bool,
    /// deb／rpm 沒有回滾檔（`allow_no_rollback` 時仍可升級）
    pub no_rollback: bool,
    /// 目標是外部 URL 的版本時，這台主機的 `install.external_url`（可升級的主機才有意義）
    pub external_url: ExternalUrl,
}

/// 主機對外部 URL 的宣告（hello 的 `install.external_url`；REMOTE.md 第 2.1 節）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExternalUrl {
    /// 目標不是外部 URL 的版本：不看
    #[default]
    NotApplicable,
    /// 沒有宣告（0.10.1 以前的 Pulse）或不認得的值：照舊算可升級，不允許的主機在推送中 `declined`
    Undeclared,
    /// 宣告有白名單（內容不回報：這個 `base_url` 是否在內不知道）
    Allowlist,
}

/// REMOTE.md 第 4.7.2 節的資格分類。跳過的原因：`already`、`unsupported_version`、`disabled`、
/// `no_trust_anchor`、`no_helper`、`unsupported`、`unsafe_path`（0.10.0）、`not_ready`（不認得的
/// `install.upgrade`）、`unsupported_platform`、`no_artifact`、
/// `artifact_missing`、`url_not_allowed`（外部 URL 的版本）、`min_upgrade_from`、
/// `no_manifest`、`downgrade_not_allowed`、`no_rollback`、`decommissioned`
pub fn classify(h: &UpgradeHost, ctx: &ReleaseContext, opts: &RolloutOptions) -> Eligibility {
    let mut e = Eligibility {
        skip: None,
        from_version: h.agent_version.clone(),
        downgrade: false,
        no_rollback: false,
        external_url: ExternalUrl::NotApplicable,
    };
    let skip = |mut e: Eligibility, why| {
        e.skip = Some(why);
        e
    };
    if h.decommissioned {
        return skip(e, "decommissioned");
    }
    let current = h
        .agent_version
        .as_deref()
        .and_then(|v| Version::parse(v).ok());
    let Some(current) = current.filter(|v| v.core() >= (0, 9, 0)) else {
        return skip(e, "unsupported_version");
    };
    if current == ctx.target {
        return skip(e, "already");
    }
    match h.install_upgrade.as_deref() {
        Some(INSTALL_UPGRADE_READY) => {}
        None => return skip(e, "unsupported_version"),
        Some("disabled") => return skip(e, "disabled"),
        Some("no_trust_anchor") => return skip(e, "no_trust_anchor"),
        Some("no_helper") => return skip(e, "no_helper"),
        Some("unsupported") => return skip(e, "unsupported"),
        // Pulse 0.10.0 起：其他條件都就緒，但安裝路徑不會通過升級輔助程式的路徑檢查（推送的話會以
        // PLS-E8405 `unsafe_path` 失敗）。0.9 的 Pulse 不會送這個值
        Some(INSTALL_UPGRADE_UNSAFE_PATH) => return skip(e, SKIP_UNSAFE_PATH),
        // 不認得的值（比這個 Kairos 新的 Pulse）
        Some(_) => return skip(e, "not_ready"),
    }
    let method = h.install_method.as_deref().unwrap_or("unknown");
    let arch = arch_of(h.os.as_deref()).unwrap_or_default();
    let Some((os, _, _)) = release::artifact_kind(method, &arch) else {
        return skip(e, "unsupported_platform");
    };
    // 0.9.0 只支援 Linux（Windows 的 MSI／zip 在 0.10，REMOTE.md 第 14 節 D15）
    if os != Os::Linux {
        return skip(e, "unsupported_platform");
    }
    if pick_artifact(&ctx.artifacts, method, &arch).is_none() {
        // 檔案遺失（已存入但不在版本庫的目錄中）與「沒有這個平台的檔案」分開：處理方式不同
        if pick_artifact(&ctx.missing, method, &arch).is_some() {
            return skip(e, SKIP_ARTIFACT_MISSING);
        }
        return skip(e, "no_artifact");
    }
    // 外部 URL 的版本：提議的發佈檔要由 Pulse 從 base_url 下載。宣告白名單是空的（`none`）的主機一定
    // 拒絕——事先分類，不必等推送中的 `declined`。沒有宣告（0.10.1 以前的 Pulse）或不認得的值照舊算可升級
    if ctx.external_base_url.is_some() {
        e.external_url = match h.install_external_url.as_deref() {
            Some(INSTALL_EXTERNAL_URL_NONE) => return skip(e, SKIP_URL_NOT_ALLOWED),
            Some(INSTALL_EXTERNAL_URL_ALLOWLIST) => ExternalUrl::Allowlist,
            _ => ExternalUrl::Undeclared,
        };
    }
    if ctx.target < current {
        if !opts.allow_downgrade {
            return skip(e, "downgrade_not_allowed");
        }
        e.downgrade = true;
    } else {
        let Some(m) = &ctx.manifest else {
            return skip(e, "no_manifest");
        };
        if Version::parse(&m.min_upgrade_from).is_ok_and(|min| current < min) {
            return skip(e, "min_upgrade_from");
        }
    }
    if matches!(method, INSTALL_METHOD_DEB | INSTALL_METHOD_RPM) {
        let has = ctx
            .by_version
            .get(&current.to_string())
            .is_some_and(|list| pick_artifact(list, method, &arch).is_some());
        if !has {
            e.no_rollback = true;
            if !opts.allow_no_rollback {
                return skip(e, "no_rollback");
            }
        }
    }
    e
}

/// 推送中的順序：先 canary，其餘依 `SHA-256(rollout_id ‖ agent_id)`（同一個推送每次結果相同）
pub fn order_key(rollout_id: &str, agent_id: &str) -> String {
    kairos_proto::enroll::sha256_hex(format!("{rollout_id}{agent_id}").as_bytes())
}

/// 分批的結果：每台可升級主機的 (批次, 批次內順序)
pub fn assign_waves(
    rollout_id: &str,
    eligible: &[&UpgradeHost],
    waves: &[WaveSpec],
    canary: &[String],
) -> Vec<(i64, i64, i64)> {
    let mut ordered: Vec<&UpgradeHost> = eligible.to_vec();
    ordered.sort_by_cached_key(|h| {
        let pos = canary.iter().position(|c| *c == h.agent_id);
        (
            pos.is_none(),
            pos.unwrap_or(0),
            order_key(rollout_id, &h.agent_id),
        )
    });
    let n = ordered.len();
    // 每批的台數：最後一批取其餘；第一批有指定 canary 時就是那些主機；其他依 size（百分比以**剩餘**
    // 的可升級主機計，至少 1 台）與 max_hosts。兩批時與 0.9 的結果相同
    let mut out = Vec::with_capacity(n);
    let mut hosts = ordered.iter();
    let mut remaining = n;
    for (wave, w) in waves.iter().enumerate() {
        let size = if wave + 1 == waves.len() {
            remaining
        } else if wave == 0 && !canary.is_empty() {
            ordered
                .iter()
                .filter(|h| canary.contains(&h.agent_id))
                .count()
        } else {
            let size = match w.size {
                WaveSize::Count(c) => c as usize,
                WaveSize::Percent(p) => (remaining * p as usize).div_ceil(100).max(1),
                WaveSize::Rest => remaining,
            };
            w.max_hosts.map_or(size, |m| size.min(m as usize))
        }
        .min(remaining);
        for (ordinal, h) in hosts.by_ref().take(size).enumerate() {
            out.push((h.host_id, wave as i64, ordinal as i64));
        }
        remaining -= size;
    }
    out
}

/// 資格分類的統計（預覽與詳情）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    pub matched: usize,
    pub eligible: usize,
    pub downgrade: usize,
    pub no_rollback: usize,
    /// 原因 → 台數
    pub skipped: BTreeMap<&'static str, usize>,
    /// 原因 → 前 100 台的 agent_id
    pub samples: BTreeMap<&'static str, Vec<(String, String)>>,
    /// 已在另一個進行中推送的主機（agent_id, rollout_id），前 100 台
    pub overlap: Vec<(String, String)>,
    pub overlap_count: usize,
    /// 每批的台數
    pub waves: Vec<usize>,
    /// 目標是外部 URL 的版本時（`None`：由 Kairos 代管）：可升級主機對外部 URL 的宣告
    pub external_url: Option<ExternalUrlPreview>,
}

/// 外部 URL 版本的預覽（API 的 `external_url`）：可升級的主機中，沒有宣告（`undeclared`）與宣告有白名單
/// （`allowlist`）的台數——兩者都可能在推送中因為白名單不含這個 `base_url` 而 `declined`
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalUrlPreview {
    pub base_url: String,
    pub undeclared: usize,
    pub allowlist: usize,
}

/// [`plan`] 的結果：統計、每台比對到的主機與資格、可升級主機的 (host_id, 批次, 批次內順序)
pub type Plan = (
    Preview,
    Vec<(UpgradeHost, Eligibility)>,
    Vec<(i64, i64, i64)>,
);

/// 預覽與送出共用：比對選擇器、分類、分批
pub fn plan(
    rollout_id: &str,
    hosts: &[UpgradeHost],
    spec_selector: &SelectorSpec,
    waves: &[WaveSpec],
    opts: &RolloutOptions,
    ctx: &ReleaseContext,
    busy: &HashMap<String, String>,
) -> Plan {
    let mut p = Preview {
        external_url: ctx.external_base_url.as_ref().map(|u| ExternalUrlPreview {
            base_url: u.clone(),
            ..ExternalUrlPreview::default()
        }),
        ..Preview::default()
    };
    let mut classified = Vec::new();
    for h in hosts.iter().filter(|h| spec_selector.matches(h)) {
        p.matched += 1;
        if let Some(r) = busy.get(&h.agent_id).filter(|r| r.as_str() != rollout_id) {
            p.overlap_count += 1;
            if p.overlap.len() < 100 {
                p.overlap.push((h.agent_id.clone(), r.clone()));
            }
        }
        let e = classify(h, ctx, opts);
        match e.skip {
            Some(why) => {
                *p.skipped.entry(why).or_default() += 1;
                let s = p.samples.entry(why).or_default();
                if s.len() < 100 {
                    s.push((h.agent_id.clone(), h.hostname.clone()));
                }
            }
            None => {
                p.eligible += 1;
                p.downgrade += usize::from(e.downgrade);
                p.no_rollback += usize::from(e.no_rollback);
                if let Some(x) = p.external_url.as_mut() {
                    x.undeclared += usize::from(e.external_url == ExternalUrl::Undeclared);
                    x.allowlist += usize::from(e.external_url == ExternalUrl::Allowlist);
                }
            }
        }
        classified.push((h.clone(), e));
    }
    let eligible: Vec<&UpgradeHost> = classified
        .iter()
        .filter(|(_, e)| e.skip.is_none())
        .map(|(h, _)| h)
        .collect();
    let assigned = assign_waves(rollout_id, &eligible, waves, &opts.canary_agent_ids);
    p.waves = vec![0; waves.len()];
    for (_, w, _) in &assigned {
        p.waves[*w as usize] += 1;
    }
    (p, classified, assigned)
}

impl Preview {
    pub fn json(&self) -> Value {
        json!({
            "matched": self.matched,
            "eligible": self.eligible,
            "downgrade": self.downgrade,
            "no_rollback": self.no_rollback,
            "skipped": self.skipped,
            "samples": self.samples.iter().map(|(k, v)| (k.to_string(), v.iter().map(|(a, h)| json!({"agent_id": a, "hostname": h})).collect::<Vec<_>>())).collect::<BTreeMap<_, _>>(),
            "overlap": {
                "count": self.overlap_count,
                "hosts": self.overlap.iter().map(|(a, r)| json!({"agent_id": a, "rollout_id": r})).collect::<Vec<_>>(),
            },
            "waves": self.waves,
            "external_url": self.external_url.as_ref().map(|x| json!({
                "base_url": x.base_url,
                "undeclared": x.undeclared,
                "allowlist": x.allowlist,
            })),
        })
    }
}

// ---- 推送引擎的一步 ----

/// 一台主機的即時狀態（狀態表與連線）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostLive {
    /// 連線中且協商了 `upgrade`
    pub capable: bool,
    /// 連線中（不論是否協商了 `upgrade`）
    pub online: bool,
    /// 離線多久（毫秒；線上或不知道時 `None`）
    pub offline_for_ms: Option<i64>,
    /// 最後一次離線是 Kairos 這端造成的（關機、節點失聯 `server_lost`）
    pub offline_by_server: bool,
}

/// 引擎對一台主機的決定
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAction {
    /// 提議（`pending` → `offered`）
    Offer,
    /// 超過 `apply_window` 仍未開始替換
    Expire,
    /// 替換後超過期限沒有回報且離線
    Unknown,
    /// 離線超過寬限
    SkipOffline,
    /// 重新上線的 `skipped_offline` 主機回到 `pending`
    Revive,
    /// 連線中但沒有協商 `upgrade`（本機停用或缺少輔助程式）：跳過
    SkipNotReady,
    /// 有升級時段的推送：時段結束時還沒開始替換（Pulse 回報 `expired`，或時段結束後仍停在提議中）
    /// → 回到 `pending`、換新的 `upgrade_id`，下一個時段重新提議
    Requeue,
}

/// 推送層級的決定
#[derive(Debug, Clone, PartialEq)]
pub enum RolloutAction {
    /// 失敗比例超過上限：自動暫停
    Pause {
        failures: usize,
        finished: usize,
        ratio: f64,
    },
    /// 這批結束，開始觀察（`soak_until`）
    StartSoak { until: i64 },
    /// 觀察期滿、需要人工繼續
    AwaitContinue,
    /// 開始下一批
    NextWave,
    /// 所有批次都結束
    Complete,
}

/// [`plan_tick`] 的輸入
#[derive(Debug, Clone)]
pub struct TickInput<'a> {
    pub now_ms: i64,
    /// Kairos 啟動的時間
    pub started_ms: i64,
    /// 啟動後不提議新主機的時間（[`START_GRACE_MS`]；e2e 測試以測試掛鉤縮短）
    pub start_grace_ms: i64,
    pub current_wave: i64,
    pub soak_until: Option<i64>,
    pub awaiting_continue: bool,
    pub waves: &'a [WaveSpec],
    pub options: &'a RolloutOptions,
    /// 排程與升級時段：時段外不提議新的主機
    pub schedule: &'a ScheduleSpec,
}

/// 推送引擎的一步（純函式）：每台主機的動作與推送層級的動作。主機動作先套用，推送動作以套用後的狀態計算
pub fn plan_tick(
    t: &TickInput<'_>,
    hosts: &[RolloutHostRow],
    live: &dyn Fn(&str) -> HostLive,
) -> (Vec<(usize, HostAction)>, Option<RolloutAction>) {
    let mut actions = Vec::new();
    let mut states: Vec<&str> = hosts.iter().map(|h| h.state.as_str()).collect();
    let windowed = !t.schedule.windows.is_empty();
    let health_ms = i64::from(t.options.health_timeout_secs) * 1000;
    let grace_ms = i64::from(t.options.offline_grace_hours) * 3_600_000;
    for (i, h) in hosts.iter().enumerate() {
        let l = live(&h.agent_id);
        let act = match h.state.as_str() {
            s if is_offered(s) && windowed => h.offered_at.and_then(|at| {
                // 提議時所在時段的結束（提議的 apply_window.not_after）
                let end = t
                    .schedule
                    .apply_window(at, APPLY_WINDOW_MS)
                    .map_or(at, |(_, e)| e);
                (t.now_ms > end + WINDOW_EXPIRE_GRACE_MS).then(|| requeue_or_expire(h))
            }),
            s if is_offered(s) => h
                .offered_at
                .filter(|at| t.now_ms > at + APPLY_WINDOW_MS + EXPIRE_GRACE_MS)
                .map(|_| HostAction::Expire),
            // Pulse 在時段結束時回報的 expired：下一個時段再提議
            HOST_EXPIRED
                if windowed
                    && h.wave >= 0
                    && h.reason.as_deref() != Some(REASON_ROLLOUT_ENDED)
                    && h.attempts < MAX_WINDOW_REQUEUES =>
            {
                Some(HostAction::Requeue)
            }
            HOST_APPLYING => {
                // Kairos 這端造成的離線（關機、server_lost）從 Kairos 啟動時重新起算，不算主機卡住
                let since = h.applying_at.unwrap_or(h.updated_at);
                let since = if l.offline_by_server {
                    since.max(t.started_ms)
                } else {
                    since
                };
                (!l.online && t.now_ms > since + health_ms + APPLY_STUCK_GRACE_MS)
                    .then_some(HostAction::Unknown)
            }
            HOST_PENDING if h.wave <= t.current_wave => {
                if l.offline_for_ms.is_some_and(|o| o > grace_ms) {
                    Some(HostAction::SkipOffline)
                } else if l.online && !l.capable {
                    Some(HostAction::SkipNotReady)
                } else {
                    None
                }
            }
            HOST_SKIPPED_OFFLINE if l.capable => Some(HostAction::Revive),
            _ => None,
        };
        if let Some(a) = act {
            states[i] = match a {
                HostAction::Expire => HOST_EXPIRED,
                HostAction::Unknown => HOST_UNKNOWN,
                HostAction::SkipOffline => HOST_SKIPPED_OFFLINE,
                HostAction::SkipNotReady => HOST_SKIPPED,
                HostAction::Revive | HostAction::Requeue => HOST_PENDING,
                HostAction::Offer => HOST_OFFERED,
            };
            actions.push((i, a));
        }
    }
    // 失敗比例（這一批，持續檢查：canary 的上限 0 時任何失敗都立即暫停）
    let wave = t.waves.get(t.current_wave as usize);
    let in_wave: Vec<usize> = (0..hosts.len())
        .filter(|i| hosts[*i].wave == t.current_wave)
        .collect();
    let failures = in_wave.iter().filter(|i| is_failure(states[**i])).count();
    let finished = in_wave
        .iter()
        .filter(|i| counts_for_ratio(states[**i]))
        .count();
    if let Some(w) = wave
        && finished > 0
    {
        let ratio = failures as f64 / finished as f64;
        if ratio > w.max_failure_ratio {
            return (
                actions,
                Some(RolloutAction::Pause {
                    failures,
                    finished,
                    ratio,
                }),
            );
        }
    }
    // 提議新的主機：Kairos 剛啟動、觀察中、等待繼續或升級時段外不提議
    let soaking = t.soak_until.is_some();
    if t.now_ms >= t.started_ms + t.start_grace_ms
        && !soaking
        && !t.awaiting_continue
        && t.schedule.window_at(t.now_ms).is_open()
    {
        let mut slots = (t.options.max_in_progress as usize)
            .saturating_sub(states.iter().filter(|s| is_in_progress(s)).count());
        let mut pending: Vec<usize> = (0..hosts.len())
            .filter(|i| states[*i] == HOST_PENDING && hosts[*i].wave <= t.current_wave)
            // 這一輪才重新排入的主機要先換 upgrade_id，下一輪再提議
            .filter(|i| !actions.contains(&(*i, HostAction::Requeue)))
            .collect();
        pending.sort_by_key(|i| (hosts[*i].wave, hosts[*i].ordinal));
        for i in pending {
            if slots == 0 {
                break;
            }
            if live(&hosts[i].agent_id).capable {
                states[i] = HOST_OFFERED;
                actions.retain(|(j, _)| *j != i);
                actions.push((i, HostAction::Offer));
                slots -= 1;
            }
        }
    }
    // 這一批是否結束
    let wave_done = in_wave.iter().all(|i| is_terminal(states[*i]))
        && hosts
            .iter()
            .enumerate()
            .filter(|(_, h)| h.wave < t.current_wave)
            .all(|(i, _)| is_terminal(states[i]));
    if !wave_done {
        return (actions, None);
    }
    let last = t.current_wave as usize + 1 >= t.waves.len();
    let soak_ms = i64::from(wave.map_or(0, |w| w.soak_minutes)) * 60_000;
    let action = match t.soak_until {
        _ if last && !soaking => Some(RolloutAction::Complete),
        None => Some(RolloutAction::StartSoak {
            until: t.now_ms + soak_ms,
        }),
        Some(until) if t.now_ms < until => None,
        Some(_) if wave.is_some_and(|w| w.manual_continue) && !t.awaiting_continue => {
            Some(RolloutAction::AwaitContinue)
        }
        Some(_) if t.awaiting_continue => None,
        Some(_) if last => Some(RolloutAction::Complete),
        Some(_) => Some(RolloutAction::NextWave),
    };
    (actions, action)
}

/// 有升級時段的推送中逾時的主機：重新排入，超過次數上限時標為 `expired`
fn requeue_or_expire(h: &RolloutHostRow) -> HostAction {
    if h.attempts < MAX_WINDOW_REQUEUES {
        HostAction::Requeue
    } else {
        HostAction::Expire
    }
}

/// 各狀態的台數
pub fn progress(hosts: &[RolloutHostRow]) -> BTreeMap<String, usize> {
    let mut m: BTreeMap<String, usize> = HOST_STATES.iter().map(|s| (s.to_string(), 0)).collect();
    for h in hosts {
        *m.entry(h.state.clone()).or_default() += 1;
    }
    m
}

/// 暫停的原因分類（API 的 `pause_cause`）：主控台暫停為 [`PAUSE_MANUAL`]
pub const PAUSE_MANUAL: &str = "manual";
/// 自動暫停：批次的失敗比例超過上限
pub const PAUSE_FAILURE_RATIO: &str = "failure_ratio";
/// 自動暫停：主機回報回滾失敗
pub const PAUSE_ROLLBACK_FAILED: &str = "rollback_failed";
/// 自動暫停：主機回報發佈包的簽章驗證失敗（同一版本的其他推送一起暫停）
pub const PAUSE_SIGNATURE_FAILED: &str = "signature_failed";

/// API 的 `pause_cause`：暫停中才有值；資料表的 `pause_cause` 為 NULL（主控台或 API 暫停）時為 `manual`
pub fn pause_cause<'a>(state: &str, cause: Option<&'a str>) -> Option<&'a str> {
    (state == ROLLOUT_PAUSED).then(|| cause.unwrap_or(PAUSE_MANUAL))
}

/// 推送清單的 `next_action`（固定代碼＋參數，主控台組文案；設計包 §31.8 F8 #4）。暫停中另附 `cause`
/// （[`pause_cause`]）
#[cfg(test)]
pub fn next_action(
    state: &str,
    soak_until: Option<i64>,
    awaiting_continue: bool,
    pause_reason: Option<&str>,
    cause: Option<&str>,
    now_ms: i64,
) -> Value {
    next_action_scheduled(
        state,
        soak_until,
        awaiting_continue,
        pause_reason,
        cause,
        &ScheduleSpec::default(),
        now_ms,
    )
}

/// [`next_action`] 加上排程：`scheduled{starts_at}`（等待開始時間）、`waiting_window{until}`（升級時段外，
/// `until` 為下一個時段開始的時間；之後沒有時段時為 `null`）
pub fn next_action_scheduled(
    state: &str,
    soak_until: Option<i64>,
    awaiting_continue: bool,
    pause_reason: Option<&str>,
    cause: Option<&str>,
    schedule: &ScheduleSpec,
    now_ms: i64,
) -> Value {
    match state {
        ROLLOUT_DRAFT => json!({"code": "draft"}),
        ROLLOUT_PENDING_APPROVAL => json!({"code": "pending_approval"}),
        ROLLOUT_SCHEDULED => json!({
            "code": "scheduled",
            "starts_at": schedule.starts_at.map(crate::api::http::ts_ms),
        }),
        ROLLOUT_PAUSED => {
            json!({"code": "paused", "reason": pause_reason, "cause": pause_cause(state, cause)})
        }
        ROLLOUT_COMPLETED => json!({"code": "completed"}),
        ROLLOUT_CANCELLED => json!({"code": "cancelled"}),
        _ if awaiting_continue => json!({"code": "waiting_continue"}),
        _ => match soak_until {
            Some(t) if t > now_ms => json!({
                "code": "soaking",
                "minutes": ((t - now_ms) as f64 / 60_000.0).ceil() as i64,
                "until": crate::api::http::ts_ms(t),
            }),
            _ => match schedule.window_at(now_ms) {
                WindowState::Closed { next } => json!({
                    "code": "waiting_window",
                    "until": next.map(crate::api::http::ts_ms),
                }),
                WindowState::Open { .. } => json!({"code": "upgrading"}),
            },
        },
    }
}

/// 送出前的確認字串（設計包 screens.md §0.3、S26）：L2「升級 N 台」；允許無法自動回滾的主機且有這類主機時
/// L2「升級 N 台（無法自動回滾 M 台）」（全形括號）；降版 L3「降版 N 台」（優先）
pub fn confirm_text(eligible: usize, downgrade: bool, no_rollback: usize) -> String {
    if downgrade {
        format!("降版 {eligible} 台")
    } else if no_rollback > 0 {
        format!("升級 {eligible} 台（無法自動回滾 {no_rollback} 台）")
    } else {
        format!("升級 {eligible} 台")
    }
}

/// 核准的確認字串（ADR-0002 決策點 2b）：L2「核准 N 台」；含降版時 L3「核准降版 N 台」。N 為已固定的
/// 可升級台數（與送出的 [`confirm_text`] 同一個數字），由伺服器依 `rollout_hosts` 計算
pub fn approve_confirm_text(eligible: usize, downgrade: bool) -> String {
    if downgrade {
        format!("核准降版 {eligible} 台")
    } else {
        format!("核准 {eligible} 台")
    }
}

/// 第一批超過可升級主機的這個比例（或 [`LARGE_FIRST_WAVE_HOSTS`] 台）時提醒 `large_first_wave`
pub const LARGE_FIRST_WAVE_PERCENT: usize = 10;
pub const LARGE_FIRST_WAVE_HOSTS: usize = 100;

/// 風險提醒的固定代碼（ADR-0002 決策點 3、B9；主控台組文案，預覽、送審摘要與核准對話框顯示同一組）：
/// `single_wave`（只有一批）、`large_first_wave`（第一批超過 10% 或 100 台）、`no_pause_between_waves`
/// （批次之間沒有觀察時間、也不需要人工繼續）、`empty_wave`（某一批是 0 台）、`downgrade`、`no_rollback`、
/// `artifact_problem`（目標版本有檔案遺失或大小不符的發佈檔）。`wave_hosts` 為每批的台數
pub fn risk_warnings(
    waves: &[WaveSpec],
    wave_hosts: &[usize],
    eligible: usize,
    downgrade: usize,
    no_rollback: usize,
    artifact_problem: bool,
) -> Vec<&'static str> {
    let mut out = Vec::new();
    if eligible > 0 {
        if waves.len() <= 1 {
            if eligible > 1 {
                out.push("single_wave");
            }
        } else {
            let first = wave_hosts.first().copied().unwrap_or(0);
            if first > LARGE_FIRST_WAVE_HOSTS || first * 100 > eligible * LARGE_FIRST_WAVE_PERCENT {
                // 只有幾台主機時第一批必然超過 10%：至少 1 台不算
                if first > 1 {
                    out.push("large_first_wave");
                }
            }
            if waves[..waves.len() - 1]
                .iter()
                .all(|w| w.soak_minutes == 0 && !w.manual_continue)
            {
                out.push("no_pause_between_waves");
            }
            if wave_hosts.contains(&0) {
                out.push("empty_wave");
            }
        }
    }
    if downgrade > 0 {
        out.push("downgrade");
    }
    if no_rollback > 0 {
        out.push("no_rollback");
    }
    if artifact_problem {
        out.push("artifact_problem");
    }
    out
}

/// 已在進行中推送的主機（agent_id → rollout_id）
pub fn busy_map(rows: Vec<(String, String)>) -> HashMap<String, String> {
    rows.into_iter().collect()
}

/// 推送詳情中各批的統計
pub fn waves_json(waves: &[WaveSpec], hosts: &[RolloutHostRow], current: i64) -> Value {
    Value::Array(
        waves
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let in_wave: Vec<&RolloutHostRow> =
                    hosts.iter().filter(|h| h.wave == i as i64).collect();
                let failures = in_wave.iter().filter(|h| is_failure(&h.state)).count();
                let finished = in_wave
                    .iter()
                    .filter(|h| counts_for_ratio(&h.state))
                    .count();
                let mut spec = w.json();
                spec["index"] = json!(i);
                spec["hosts"] = json!(in_wave.iter().filter(|h| h.state != HOST_SKIPPED).count());
                spec["progress"] =
                    json!(progress(&in_wave.into_iter().cloned().collect::<Vec<_>>()));
                spec["failure_ratio"] = json!(if finished > 0 {
                    failures as f64 / finished as f64
                } else {
                    0.0
                });
                spec["current"] = json!(i as i64 == current);
                spec
            })
            .collect(),
    )
}

/// 主機欄位的 JSON（推送的主機表、主機的升級紀錄）
pub fn host_json(h: &RolloutHostRow) -> Value {
    let ts = |v: Option<i64>| v.map(crate::api::http::ts_ms);
    json!({
        "rollout_id": h.rollout_id,
        "agent_id": h.agent_id,
        "hostname": h.hostname,
        "upgrade_id": h.upgrade_id,
        "wave": h.wave,
        "ordinal": h.ordinal,
        "state": h.state,
        "reason": h.reason,
        "from_version": h.from_version,
        "to_version": h.to_version,
        "attempts": h.attempts,
        "progress": h.bytes_total.map(|t| json!({"bytes": h.bytes_done.unwrap_or(0), "total": t})),
        "code": h.code,
        "detail": h.detail,
        "offered_at": ts(h.offered_at),
        "applying_at": ts(h.applying_at),
        "finished_at": ts(h.finished_at),
        "updated_at": crate::api::http::ts_ms(h.updated_at),
        "acknowledged_at": ts(h.acknowledged_at),
        "acknowledged_by": h.acknowledged_by,
    })
}

#[cfg(test)]
#[path = "rollout_tests.rs"]
mod tests;
