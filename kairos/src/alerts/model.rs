//! 告警的資料模型：嚴重度、處理類型、事件狀態、規則、事件、靜音、通知管道（`kairos/ALERTS.md` 第 4 節）。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 嚴重度四級（設計包 `alerts-ux.md` §2.1、INTEGRATION D3）。`Ord` 依嚴重程度：緊急最小（排序在前）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Emergency,
    Critical,
    Warning,
    Info,
}

impl Severity {
    pub const ALL: [Severity; 4] = [
        Severity::Emergency,
        Severity::Critical,
        Severity::Warning,
        Severity::Info,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Emergency => "emergency",
            Severity::Critical => "critical",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }

    /// 標籤（`copywriting.md` §2.2，不得改寫）
    pub fn label(self) -> &'static str {
        match self {
            Severity::Emergency => "緊急",
            Severity::Critical => "嚴重",
            Severity::Warning => "警告",
            Severity::Info => "資訊",
        }
    }

    /// 排序用：緊急 0 → 資訊 3
    pub fn rank(self) -> i64 {
        match self {
            Severity::Emergency => 0,
            Severity::Critical => 1,
            Severity::Warning => 2,
            Severity::Info => 3,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.as_str() == s)
    }

    /// 預設的處理類型：資訊一律為紀錄，其他為需要處理（規劃提醒由規則指定，§2.3）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn default_attention(self) -> Attention {
        if self == Severity::Info {
            Attention::Record
        } else {
            Attention::Action
        }
    }
}

/// 處理類型（`alerts-ux.md` §2.3）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    /// 需要處理
    Action,
    /// 規劃提醒：不推播、不需確認，條件解除時自動結束
    Advisory,
    /// 紀錄：不推播、不需確認
    Record,
}

impl Attention {
    pub fn as_str(self) -> &'static str {
        match self {
            Attention::Action => "action",
            Attention::Advisory => "advisory",
            Attention::Record => "record",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Attention::Action, Attention::Advisory, Attention::Record]
            .into_iter()
            .find(|x| x.as_str() == s)
    }
}

/// 事件狀態（`alerts-ux.md` §5.1）。「已恢復」再以 `acked` 區分未確認／已確認
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventState {
    Firing,
    Acknowledged,
    Recovered,
    Resolved,
}

impl EventState {
    pub fn as_str(self) -> &'static str {
        match self {
            EventState::Firing => "firing",
            EventState::Acknowledged => "acknowledged",
            EventState::Recovered => "recovered",
            EventState::Resolved => "resolved",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EventState::Firing => "觸發中",
            EventState::Acknowledged => "已確認",
            EventState::Recovered => "已恢復",
            EventState::Resolved => "已解決",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            EventState::Firing,
            EventState::Acknowledged,
            EventState::Recovered,
            EventState::Resolved,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
    }

    /// 條件仍成立（觸發中或已確認）
    pub fn is_active(self) -> bool {
        matches!(self, EventState::Firing | EventState::Acknowledged)
    }
}

/// 規則的類別（S08「類型」；靜音的時長上限只把 `System` 視為系統規則，`alerts-ux.md` §4.3）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Host,
    Identity,
    Aggregate,
    System,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Host => "host",
            Category::Identity => "identity",
            Category::Aggregate => "aggregate",
            Category::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Category::Host,
            Category::Identity,
            Category::Aggregate,
            Category::System,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
    }
}

/// 根因類別（S07「解決」；後四個由系統填入）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootCause {
    Hardware,
    Network,
    UnregisteredMaintenance,
    Software,
    FalsePositive,
    Other,
    /// 已恢復且未確認，24 小時後自動結案
    SelfRecovered,
    /// 規劃提醒、紀錄：條件解除（自動）
    ConditionCleared,
    /// 已恢復、已確認但 7 天未處理，自動結案
    AutoClosed,
    /// 主機已除役
    Decommissioned,
}

impl RootCause {
    pub fn as_str(self) -> &'static str {
        match self {
            RootCause::Hardware => "hardware",
            RootCause::Network => "network",
            RootCause::UnregisteredMaintenance => "unregistered_maintenance",
            RootCause::Software => "software",
            RootCause::FalsePositive => "false_positive",
            RootCause::Other => "other",
            RootCause::SelfRecovered => "self_recovered",
            RootCause::ConditionCleared => "condition_cleared",
            RootCause::AutoClosed => "auto_closed",
            RootCause::Decommissioned => "decommissioned",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RootCause::Hardware => "硬體",
            RootCause::Network => "網路",
            RootCause::UnregisteredMaintenance => "計畫性作業未登記",
            RootCause::Software => "軟體",
            RootCause::FalsePositive => "誤報",
            RootCause::Other => "其他",
            RootCause::SelfRecovered => "自行恢復",
            RootCause::ConditionCleared => "條件解除（自動）",
            RootCause::AutoClosed => "已恢復，7 天未處理而自動結案",
            RootCause::Decommissioned => "主機已除役",
        }
    }

    /// 使用者可以選的根因（S07）
    pub const USER: [RootCause; 6] = [
        RootCause::Hardware,
        RootCause::Network,
        RootCause::UnregisteredMaintenance,
        RootCause::Software,
        RootCause::FalsePositive,
        RootCause::Other,
    ];

    pub fn parse_user(s: &str) -> Option<Self> {
        Self::USER.into_iter().find(|x| x.as_str() == s)
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            RootCause::Hardware,
            RootCause::Network,
            RootCause::UnregisteredMaintenance,
            RootCause::Software,
            RootCause::FalsePositive,
            RootCause::Other,
            RootCause::SelfRecovered,
            RootCause::ConditionCleared,
            RootCause::AutoClosed,
            RootCause::Decommissioned,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
    }
}

/// 範圍（靜音、規則共用；同一欄位的多個值取「或」，不同欄位之間取「且」，全部不填＝全部主機，D23）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scope {
    /// 標籤（完全相同，不分大小寫）
    pub tags: Vec<String>,
    /// 網段（CIDR，主機位元必須為 0）
    pub subnets: Vec<String>,
    /// 主機（agent_id，小寫）
    pub hosts: Vec<String>,
}

impl Scope {
    pub fn is_all(&self) -> bool {
        self.tags.is_empty() && self.subnets.is_empty() && self.hosts.is_empty()
    }
}

/// 規則的通知設定；`None` 的欄位使用嚴重度的預設（`alerts-ux.md` §2.1.1）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyPolicy {
    /// 未確認時的重送間隔（秒）
    pub repeat_interval_secs: Option<u64>,
    /// 重送次數上限（不含第一次）
    pub repeat_max: Option<u32>,
    /// 恢復時通知
    pub notify_recovery: Option<bool>,
    /// 觸發多久仍未確認時通知升級對象（秒）
    pub escalate_after_secs: Option<u64>,
    /// 升級對象（通知管道）
    pub escalate_channels: Vec<String>,
}

/// 依嚴重度套用預設後的通知設定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveNotify {
    pub repeat_interval_secs: Option<u64>,
    pub repeat_max: u32,
    pub notify_recovery: bool,
    pub escalate_after_secs: Option<u64>,
}

/// 緊急的重送間隔上限（`alerts-ux.md` §2.1.1）
pub const EMERGENCY_MAX_REPEAT_SECS: u64 = 30 * 60;

impl NotifyPolicy {
    pub fn effective(&self, severity: Severity) -> EffectiveNotify {
        let (interval, max, recovery, escalate) = match severity {
            Severity::Emergency => (Some(15 * 60), 8, true, Some(15 * 60)),
            Severity::Critical => (Some(60 * 60), 3, true, Some(60 * 60)),
            Severity::Warning => (None, 0, false, None),
            Severity::Info => (None, 0, false, None),
        };
        let interval = self.repeat_interval_secs.or(interval);
        let interval = match (severity, interval) {
            (Severity::Emergency, Some(i)) => Some(i.min(EMERGENCY_MAX_REPEAT_SECS)),
            (_, i) => i,
        };
        EffectiveNotify {
            repeat_interval_secs: interval.filter(|i| *i > 0),
            repeat_max: self.repeat_max.unwrap_or(max),
            // 緊急的恢復通知一律發送
            notify_recovery: severity == Severity::Emergency
                || self.notify_recovery.unwrap_or(recovery),
            escalate_after_secs: if self.escalate_channels.is_empty() {
                None
            } else {
                self.escalate_after_secs.or(escalate)
            },
        }
    }
}

/// 一條規則（`alert_rules` 的一列）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub rule_id: String,
    pub builtin: bool,
    pub kind: String,
    pub category: Category,
    pub name: String,
    pub enabled: bool,
    pub severity: Severity,
    pub params: Value,
    pub scope: Scope,
    pub channels: Vec<String>,
    pub notify: NotifyPolicy,
    pub description: Option<String>,
    pub disabled_reason: Option<String>,
    pub created_at: i64,
    pub created_by: String,
    pub updated_at: i64,
    pub updated_by: String,
}

impl Rule {
    /// 這條規則可能產生的最高嚴重度（依剩餘天數升級的系統規則為緊急；靜音上限用）
    pub fn max_severity(&self) -> Severity {
        match self.params.get("escalates_to").and_then(Value::as_str) {
            Some(s) => Severity::parse(s)
                .unwrap_or(self.severity)
                .min(self.severity),
            None => self.severity,
        }
    }

    /// 處理類型（規則決定；資訊一律為紀錄）
    pub fn attention(&self) -> Attention {
        if self.severity == Severity::Info {
            return Attention::Record;
        }
        match self.params.get("attention").and_then(Value::as_str) {
            Some(a) => Attention::parse(a).unwrap_or(Attention::Action),
            None => Attention::Action,
        }
    }

    pub fn param_u64(&self, key: &str, default: u64) -> u64 {
        self.params
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or(default)
    }

    pub fn param_f64(&self, key: &str, default: f64) -> f64 {
        self.params
            .get(key)
            .and_then(Value::as_f64)
            .unwrap_or(default)
    }
}

/// 通知的進度（`alert_events.notify_state`）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyState {
    /// 已排入的觸發通知次數（第一次＋重送）
    pub sends: u32,
    pub last_sent_at: Option<i64>,
    /// 已排入過觸發通知（恢復、確認通知只在之前通知過時才送）
    pub notified: bool,
    pub escalated: bool,
    /// 彙總事件最近一次通知時的台數級距（10、50、100、500）
    pub count_step: u64,
    /// 已通知過的最高嚴重度（依天數升級的系統事件）
    pub notified_rank: Option<i64>,
    /// 抖動摘要最近一次的時間
    pub flap_summary_at: Option<i64>,
    /// 預計處理時間到期的提醒已送出
    pub eta_reminded: bool,
}

/// 一個事件（`alert_events` 的一列）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub event_id: String,
    pub rule_id: String,
    pub dedup_key: String,
    pub kind: String,
    pub category: Category,
    pub severity: Severity,
    pub attention: Attention,
    pub state: EventState,
    pub acked: bool,
    pub title: String,
    /// 對象：主機事件為 agent_id，網段事件為網段，系統事件為檢查項目
    pub subject: String,
    pub agent_id: Option<String>,
    pub code: Option<String>,
    pub parent_id: Option<String>,
    /// 同一個對象的同一段異常（主機離線的開始時間等）；手動解決後同一段異常不再重新開啟
    pub episode: Option<i64>,
    pub details: Value,
    pub started_at: i64,
    pub updated_at: i64,
    pub last_signal_at: i64,
    pub recovered_at: Option<i64>,
    pub resolved_at: Option<i64>,
    pub acked_at: Option<i64>,
    pub acked_by: Option<String>,
    pub assignee: Option<String>,
    pub eta_at: Option<i64>,
    pub root_cause: Option<RootCause>,
    pub resolve_note: Option<String>,
    pub resolved_by: Option<String>,
    pub occurrences: u64,
    pub reopen_count: u64,
    pub flapping: bool,
    pub silenced: bool,
    pub in_maintenance: bool,
    pub snooze_until: Option<i64>,
    pub notify: NotifyState,
}

impl Event {
    /// API 與 SSE 的摘要
    pub fn summary_json(&self) -> Value {
        json!({
            "event_id": self.event_id,
            "rule_id": self.rule_id,
            "kind": self.kind,
            "category": self.category,
            "severity": self.severity,
            "attention": self.attention,
            "state": self.state,
            "acked": self.acked,
            "title": self.title,
            "subject": self.subject,
            "agent_id": self.agent_id,
            "code": self.code,
            "parent_id": self.parent_id,
            "silenced": self.silenced,
            "in_maintenance": self.in_maintenance,
            "flapping": self.flapping,
            "started_at": crate::api::http::ts_ms(self.started_at),
            "updated_at": crate::api::http::ts_ms(self.updated_at),
            "recovered_at": self.recovered_at.map(crate::api::http::ts_ms),
            "resolved_at": self.resolved_at.map(crate::api::http::ts_ms),
            "assignee": self.assignee,
            "occurrences": self.occurrences,
            "reopen_count": self.reopen_count,
        })
    }
}

/// 靜音的種類（`alerts-ux.md` §4.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SilenceKind {
    /// 臨時靜音：有時長上限（7 天／系統規則 24 小時／緊急 4 小時取最短，D26）
    Silence,
    /// 維護時段：計畫性作業，不受上限，但不抑制系統事件
    Maintenance,
}

impl SilenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SilenceKind::Silence => "silence",
            SilenceKind::Maintenance => "maintenance",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [SilenceKind::Silence, SilenceKind::Maintenance]
            .into_iter()
            .find(|x| x.as_str() == s)
    }
}

/// 一個靜音或維護時段（`alert_silences` 的一列）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Silence {
    pub silence_id: String,
    pub kind: SilenceKind,
    pub name: String,
    pub scope: Scope,
    /// 規則（不選＝所有規則；系統事件只有在這裡明確選取時才會被靜音）
    pub rules: Vec<String>,
    pub starts_at: i64,
    pub ends_at: i64,
    pub reason: String,
    pub notify_on_end: bool,
    pub exclude_uptime: bool,
    pub from_event: Option<String>,
    pub created_at: i64,
    pub created_by: String,
    pub updated_at: i64,
    pub updated_by: String,
    pub ended_at: Option<i64>,
    pub ended_by: Option<String>,
    pub end_reason: Option<String>,
    pub end_notified: bool,
}

impl Silence {
    /// 實際結束的時間（提前結束時為 `ended_at`）
    pub fn effective_end(&self) -> i64 {
        self.ended_at.map_or(self.ends_at, |e| e.min(self.ends_at))
    }

    pub fn is_active(&self, now_ms: i64) -> bool {
        self.starts_at <= now_ms && now_ms < self.effective_end()
    }

    /// `active`、`upcoming`、`ended`
    pub fn status(&self, now_ms: i64) -> &'static str {
        if self.is_active(now_ms) {
            "active"
        } else if now_ms < self.starts_at && self.ended_at.is_none() {
            "upcoming"
        } else {
            "ended"
        }
    }
}

/// 通知管道的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Webhook,
    Email,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Webhook => "webhook",
            ChannelKind::Email => "email",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [ChannelKind::Webhook, ChannelKind::Email]
            .into_iter()
            .find(|x| x.as_str() == s)
    }
}

/// 一個通知管道（`alert_channels` 的一列；機密另存，見 `secrets`）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Channel {
    pub channel_id: String,
    pub name: String,
    pub kind: ChannelKind,
    pub enabled: bool,
    /// 不含機密的設定
    pub config: Value,
    /// 解密後的機密（只在發送時使用；不序列化）
    #[serde(skip)]
    pub secrets: Value,
    /// `ok`／`failed`
    pub state: String,
    pub consecutive_failures: u32,
    pub failed_since: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub last_error_code: Option<String>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub created_by: String,
    pub updated_at: i64,
    pub updated_by: String,
}

impl Channel {
    pub fn failed(&self) -> bool {
        self.state == "failed"
    }

    /// 是否接收這個嚴重度的通知：警告只送 Email（`alerts-ux.md` §2.1.1；Webhook 管道可以設定
    /// `min_severity` 改變），資訊不推播
    pub fn accepts(&self, severity: Severity) -> bool {
        if severity == Severity::Info {
            return false;
        }
        let min = self
            .config
            .get("min_severity")
            .and_then(Value::as_str)
            .and_then(Severity::parse)
            .unwrap_or(match self.kind {
                ChannelKind::Email => Severity::Warning,
                ChannelKind::Webhook => Severity::Critical,
            });
        severity <= min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_defaults_follow_the_design_table() {
        let p = NotifyPolicy::default();
        let e = p.effective(Severity::Emergency);
        assert_eq!(e.repeat_interval_secs, Some(900));
        assert_eq!(e.repeat_max, 8);
        assert!(e.notify_recovery);
        // 沒有升級對象時不升級
        assert_eq!(e.escalate_after_secs, None);
        let c = p.effective(Severity::Critical);
        assert_eq!((c.repeat_interval_secs, c.repeat_max), (Some(3600), 3));
        let w = p.effective(Severity::Warning);
        assert_eq!((w.repeat_interval_secs, w.repeat_max), (None, 0));
        assert!(!w.notify_recovery);
        // 緊急的重送間隔上限 30 分鐘；恢復通知不能關閉
        let custom = NotifyPolicy {
            repeat_interval_secs: Some(7200),
            notify_recovery: Some(false),
            escalate_channels: vec!["ch".into()],
            ..NotifyPolicy::default()
        };
        let e = custom.effective(Severity::Emergency);
        assert_eq!(e.repeat_interval_secs, Some(1800));
        assert!(e.notify_recovery);
        assert_eq!(e.escalate_after_secs, Some(900));
        assert!(!custom.effective(Severity::Critical).notify_recovery);
        assert!(Severity::Emergency < Severity::Critical);
        assert_eq!(Severity::parse("warning"), Some(Severity::Warning));
        assert_eq!(Severity::Info.default_attention(), Attention::Record);
    }

    #[test]
    fn channel_severity_filter() {
        let mut ch = Channel {
            channel_id: "c".into(),
            name: "n".into(),
            kind: ChannelKind::Email,
            enabled: true,
            config: json!({}),
            secrets: json!({}),
            state: "ok".into(),
            consecutive_failures: 0,
            failed_since: None,
            last_attempt_at: None,
            last_success_at: None,
            last_error_code: None,
            last_error: None,
            created_at: 0,
            created_by: String::new(),
            updated_at: 0,
            updated_by: String::new(),
        };
        assert!(ch.accepts(Severity::Warning));
        assert!(!ch.accepts(Severity::Info));
        ch.kind = ChannelKind::Webhook;
        assert!(!ch.accepts(Severity::Warning));
        assert!(ch.accepts(Severity::Critical));
        ch.config = json!({"min_severity": "warning"});
        assert!(ch.accepts(Severity::Warning));
    }

    #[test]
    fn silence_status() {
        let s = Silence {
            silence_id: "s".into(),
            kind: SilenceKind::Silence,
            name: "n".into(),
            scope: Scope::default(),
            rules: vec![],
            starts_at: 100,
            ends_at: 200,
            reason: "r".into(),
            notify_on_end: true,
            exclude_uptime: false,
            from_event: None,
            created_at: 0,
            created_by: String::new(),
            updated_at: 0,
            updated_by: String::new(),
            ended_at: None,
            ended_by: None,
            end_reason: None,
            end_notified: false,
        };
        assert_eq!(s.status(50), "upcoming");
        assert_eq!(s.status(150), "active");
        assert_eq!(s.status(200), "ended");
        let ended = Silence {
            ended_at: Some(120),
            ..s
        };
        assert_eq!(ended.status(150), "ended");
        assert_eq!(ended.effective_end(), 120);
    }
}
