//! 告警引擎的狀態機（`kairos/ALERTS.md` 第 5 節；設計包 `alerts-ux.md` §3～§5）。
//!
//! 引擎本身是同步、不做 I/O 的狀態機（時間由呼叫端傳入，方便測試）：
//!
//! - 輸入：每個 tick 的 [`World`]（主機的即時狀態、系統檢查的結果、握手時被拒的憑證）、匯流排的事件
//!   （[`Engine::on_bus`]）、使用者的動作（確認、指派、暫停通知、解決、註解）。
//! - 判定為**準位觸發**：每個 tick 算出每條規則「目前成立的對象」，與進行中的事件比對——成立且沒有事件
//!   時開新事件（或在恢復 10 分鐘內重新開啟），事件的對象不再成立時轉為已恢復。重新啟動後從資料庫載回
//!   進行中的事件，同一套比對自然延續。
//! - 輸出：[`Engine::take_output`] 取出有變化的事件（整列寫回）、處理紀錄、要送出的通知與 SSE 的變化，
//!   由服務（`alerts::service`）寫入資料庫並交給通知的派送。
//!
//! 關聯彙總的三層模型（§3.3）：系統事件 → 彙總事件（網段、大規模、重新啟動後未恢復）→ 主機事件。
//! 上層成立時，原因相符的下層事件成為子事件（`parent_id`）：照常記錄、不通知、不計入徽章。

use chrono::FixedOffset;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;

use super::model::{
    Attention, Category, Channel, Event, EventState, NotifyState, RootCause, Rule, Severity,
    Silence, SilenceKind,
};
use super::rules::{ids, kind};
use super::scope::{Compiled, HostFacts};
use crate::bus::{BusEvent, DisconnectReason, EventKind};
use crate::per_ip::cidr::Cidr;

/// 恢復後這麼久之內再次觸發時重新開啟原事件（§3.2）
pub const REOPEN_WINDOW_MS: i64 = 10 * 60_000;
/// 抖動偵測的時間窗與次數（§3.2）
pub const FLAP_WINDOW_MS: i64 = 30 * 60_000;
pub const FLAP_TRANSITIONS: usize = 4;
/// 已恢復且未確認：24 小時後自動結案（緊急除外）；已確認：7 天後自動結案（§5.1）
pub const AUTO_CLOSE_UNACKED_MS: i64 = 24 * 3_600_000;
pub const AUTO_CLOSE_ACKED_MS: i64 = 7 * 24 * 3_600_000;
/// 彙總事件吸收的子事件中這個比例以上恢復時，父事件轉為已恢復（§3.3 C）
pub const PARENT_RECOVERY_RATIO: f64 = 0.9;
/// 彙總事件在台數跨越這些值時發送更新通知（§3.3 C）
pub const COUNT_STEPS: [u64; 4] = [10, 50, 100, 500];
/// 重新啟動前「在線上」的判定：清冊的 last_seen 在上次心跳前這麼久之內
pub const PREVIOUS_ONLINE_SLACK_MS: i64 = 120_000;
/// 暖機期在這個比例的主機重新連上時提早結束（§3.3 B）
pub const WARMUP_RECONNECT_RATIO: f64 = 0.98;
/// 通知中最多列出的對象（§3.4）
pub const MAX_LISTED: usize = 10;
/// 網段事件成立後先等這麼久再通知：期間若成為大規模離線的子事件就不另外通知（「先彙總再通知」，§1、§3.3 D）
pub const AGGREGATE_HOLD_MS: i64 = 60_000;

/// 引擎的設定
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub offset: FixedOffset,
    pub prefix_v4: u8,
    pub prefix_v6: u8,
    pub console_url: String,
}

/// 一台主機的狀態（清冊＋狀態表合併）
#[derive(Debug, Clone)]
pub struct HostView {
    pub agent_id: Arc<str>,
    pub hostname: Arc<str>,
    pub display_name: Option<Arc<str>>,
    /// 小寫
    pub tags: Vec<String>,
    pub ip: Option<IpAddr>,
    pub online: bool,
    /// 離線的時間（Unix 毫秒）；線上時為 `None`
    pub offline_at: Option<i64>,
    pub offline_reason: Option<DisconnectReason>,
    /// 這次啟動後是否連上過
    pub seen_since_start: bool,
    pub decommissioned: bool,
    pub connected_at: Option<i64>,
    pub last_seen: i64,
    pub offline_after_ms: u64,
    pub report_interval_ms: Option<u64>,
    /// 最近一次 report 收到的時間
    pub last_report_at: Option<i64>,
    /// 最近一次 report 的 CPU 使用率、記憶體使用比例（百分比）
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    /// 最近一次 report 的 F4 指標（磁碟、置換空間、負載、網路速率；METRICS.md 第 11.1 節）
    pub detail: Option<Arc<crate::registry::LastReport>>,
    /// CPU 邏輯處理器數（資產；每核心負載）
    pub logical_cores: Option<u32>,
}

impl HostView {
    pub fn facts(&self) -> HostFacts {
        HostFacts {
            agent_id: self.agent_id.to_string(),
            tags: self.tags.clone(),
            ip: self.ip,
        }
    }

    pub fn name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.hostname)
    }
}

/// 握手時被拒的憑證（`tls::rejections`）
#[derive(Debug, Clone)]
pub struct Rejection {
    pub agent_id: String,
    pub at_ms: i64,
    /// `revoked`、`expired`、`revocation_status_unavailable`、`other`、`auth_failed`
    pub reason: String,
    pub code: &'static str,
    pub serial: Option<String>,
    pub ip: Option<IpAddr>,
    /// 由管理員撤銷的憑證（預期行為，不觸發）
    pub admin_revoked: bool,
}

/// 系統檢查的一個成立項目（`alerts::probe`）
#[derive(Debug, Clone)]
pub struct SystemCondition {
    pub rule_id: &'static str,
    pub subject: String,
    pub severity: Severity,
    pub title: String,
    pub code: Option<&'static str>,
    pub details: Value,
}

/// 一個 tick 的輸入
#[derive(Debug, Clone, Default)]
pub struct World {
    pub now_ms: i64,
    pub hosts: Vec<HostView>,
    pub conditions: Vec<SystemCondition>,
    pub rejections: Vec<Rejection>,
    /// 這一輪系統檢查涵蓋的規則（沒有涵蓋的規則不改變狀態，例如沒有註冊服務時的註冊端點檢查）
    pub checked: Vec<&'static str>,
}

/// 處理紀錄的一筆
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    pub event_id: String,
    pub at: i64,
    pub actor: String,
    pub action: &'static str,
    pub details: Value,
}

/// 要送出的通知
#[derive(Debug, Clone, PartialEq)]
pub struct NewDelivery {
    pub event_id: Option<String>,
    pub channel_id: String,
    /// `firing`、`repeat`、`escalation`、`reopen`、`update`、`recovery`、`ack`、`flapping`、`detached`、
    /// `silence_end`、`downtime`
    pub kind: &'static str,
    /// 緊急的第一則通知：不排隊、不併入「另有 N 則」（§3.4）
    pub urgent: bool,
    pub payload: Value,
    /// 同一組（同一個管道）合併成一則
    pub group: Option<String>,
    pub created_at: i64,
}

/// SSE 的變化
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// `alert.created`、`alert.updated`、`alert.resolved`
    pub kind: &'static str,
    pub event: Value,
}

/// [`Engine::take_output`] 的結果
#[derive(Debug, Default)]
pub struct Output {
    pub events: Vec<Event>,
    pub timeline: Vec<Timeline>,
    pub deliveries: Vec<NewDelivery>,
    pub changes: Vec<Change>,
    /// 已送出「結束時仍有異常」通知的靜音
    pub silences_notified: Vec<String>,
}

impl Output {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.timeline.is_empty()
            && self.deliveries.is_empty()
            && self.changes.is_empty()
            && self.silences_notified.is_empty()
    }
}

/// 使用者動作的錯誤
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionError {
    /// `event_not_found`、`invalid_state`、`not_acknowledgeable`、`invalid_parameter`
    pub code: &'static str,
    pub message: String,
}

fn action_error(code: &'static str, message: impl Into<String>) -> ActionError {
    ActionError {
        code,
        message: message.into(),
    }
}

/// 告警暖機期（§3.3 B）
#[derive(Debug, Clone, Default)]
pub struct Warmup {
    pub until_ms: i64,
    /// 重新啟動前在線上的主機
    pub expected: HashSet<String>,
    pub ended: bool,
    /// 暖機期結束時仍未恢復的主機（重新啟動後未恢復連線事件的子事件）
    pub missing: HashSet<String>,
}

/// 啟動時從資料庫載回的狀態
#[derive(Debug, Default)]
pub struct Loaded {
    pub rules: Vec<Rule>,
    pub silences: Vec<Silence>,
    pub channels: Vec<Channel>,
    /// 尚未解決的事件
    pub open_events: Vec<Event>,
    /// 手動解決時條件仍成立的對象：去重鍵 → 異常的開始時間
    pub suppressed: HashMap<String, i64>,
    /// 事件編號的流水號：日期（YYYYMMDD）→ 已用到的最大值
    pub seq_by_day: HashMap<String, u64>,
}

/// 一個訊號（規則對某個對象成立）
#[derive(Debug, Clone)]
pub(super) struct Signal {
    pub severity: Severity,
    pub title: String,
    pub code: Option<String>,
    pub agent_id: Option<String>,
    pub details: Value,
    pub episode: Option<i64>,
    pub parent: Option<String>,
}

/// 指標門檻的持續時間
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Streak {
    /// 觸發條件從何時開始連續成立
    pub since: Option<i64>,
    /// 恢復條件從何時開始連續成立
    pub clear_since: Option<i64>,
}

/// 告警引擎
pub struct Engine {
    pub(super) cfg: EngineConfig,
    pub(super) started_ms: i64,
    pub(super) rules: Vec<Rule>,
    pub(super) compiled: HashMap<String, Compiled>,
    pub(super) silences: Vec<(Silence, Compiled)>,
    pub(super) channels: HashMap<String, Channel>,
    pub(super) events: HashMap<String, Event>,
    pub(super) by_dedup: HashMap<String, String>,
    pub(super) by_rule: HashMap<String, HashSet<String>>,
    pub(super) children: HashMap<String, BTreeSet<String>>,
    pub(super) suppressed: HashMap<String, i64>,
    pub(super) streaks: HashMap<String, Streak>,
    pub(super) superseded: HashMap<String, VecDeque<(i64, Option<IpAddr>)>>,
    pub(super) auth_failed: HashMap<String, (i64, IpAddr)>,
    pub(super) decommissions: VecDeque<(i64, String)>,
    pub(super) evictions: VecDeque<i64>,
    pub(super) tls_failed: Option<(i64, bool, String)>,
    /// 稽核日誌的自我驗證失敗：(時間, 原因, 斷點的 id, 說明)
    pub(super) audit_failed: Option<(i64, String, Option<i64>, String)>,
    /// 遠端管理的系統檢查：(規則, 對象) → (開始時間, 標題, 細節)
    pub(super) remote_conditions:
        std::collections::BTreeMap<(&'static str, String), (i64, String, Value)>,
    /// 升級中的主機（遠端管理服務發布的 `UpgradeWindow`）：agent_id → (推送, 離線告警抑制的期限)
    pub(super) upgrading: HashMap<String, (String, i64)>,
    /// 升級期間離線而沒有開事件的主機：agent_id → (推送, 抑制的期限, 那一次離線的時間)。主機恢復連線時
    /// 移除；期限過後**同一次離線**仍未恢復而開事件時，在事件的細節與處理紀錄註明「升級中」
    pub(super) upgrade_notes: HashMap<String, (String, i64, i64)>,
    pub(super) flaps: HashMap<String, VecDeque<i64>>,
    pub(super) warmup: Warmup,
    pub(super) seq_by_day: HashMap<String, u64>,
    pub(super) last_hosts: Arc<Vec<HostView>>,
    /// agent_id → `last_hosts` 中的位置（每個 tick 重建）
    pub(super) host_index: HashMap<Arc<str>, usize>,
    pub(super) active_silences: HashSet<String>,
    pub(super) out: Output,
    pub(super) dirty: HashSet<String>,
    /// 叢集模式：這次啟動是從另一個節點接手 leader（[`Engine::set_handover`]）
    pub(super) handover: Option<Handover>,
}

/// 叢集的 leader 換手（`kairos/HA.md` 第 13.2 節）：上一個心跳是另一個節點寫的
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handover {
    /// 上一個 leader
    pub from: String,
    /// 這個節點
    pub to: String,
    /// 上一個 leader 的心跳還很新（`lease_ttl_secs + 60 秒` 內）：正常的換手，不是「沒有 leader」
    pub recent: bool,
}

/// 去重鍵：`規則 ID|對象`
pub fn dedup_key(rule_id: &str, subject: &str) -> String {
    format!("{rule_id}|{subject}")
}

impl Engine {
    pub fn new(cfg: EngineConfig, now_ms: i64, warmup: Warmup, loaded: Loaded) -> Self {
        let mut e = Self {
            cfg,
            started_ms: now_ms,
            rules: Vec::new(),
            compiled: HashMap::new(),
            silences: Vec::new(),
            channels: HashMap::new(),
            events: HashMap::new(),
            by_dedup: HashMap::new(),
            by_rule: HashMap::new(),
            children: HashMap::new(),
            suppressed: loaded.suppressed,
            streaks: HashMap::new(),
            superseded: HashMap::new(),
            auth_failed: HashMap::new(),
            decommissions: VecDeque::new(),
            evictions: VecDeque::new(),
            tls_failed: None,
            audit_failed: None,
            remote_conditions: std::collections::BTreeMap::new(),
            upgrading: HashMap::new(),
            upgrade_notes: HashMap::new(),
            flaps: HashMap::new(),
            warmup,
            seq_by_day: loaded.seq_by_day,
            last_hosts: Arc::new(Vec::new()),
            host_index: HashMap::new(),
            active_silences: HashSet::new(),
            out: Output::default(),
            dirty: HashSet::new(),
            handover: None,
        };
        e.set_channels(loaded.channels);
        e.set_silences(loaded.silences);
        let mut open = loaded.open_events;
        // 先放父事件再放子事件（索引的順序無關，但保持穩定）
        open.sort_by_key(|ev| (ev.parent_id.is_some(), ev.started_at));
        for ev in open {
            e.index(&ev);
            e.events.insert(ev.event_id.clone(), ev);
        }
        e.rules = loaded.rules;
        for r in &mut e.rules {
            super::rules::enforce_protected(r);
        }
        e.recompile();
        e
    }

    // ------------------------------------------------------------------
    // 設定（規則、靜音、管道）
    // ------------------------------------------------------------------

    fn recompile(&mut self) {
        self.compiled = self
            .rules
            .iter()
            .map(|r| (r.rule_id.clone(), Compiled::new(&r.scope)))
            .collect();
    }

    /// 換掉規則；停用的規則的進行中事件轉為已恢復（不再判定）
    pub fn set_rules(&mut self, mut rules: Vec<Rule>, now: i64) {
        // 不可停用的系統規則：資料庫那一列被停用也照常判定（rules::enforce_protected）
        for r in &mut rules {
            super::rules::enforce_protected(r);
        }
        let disabled: Vec<String> = rules
            .iter()
            .filter(|r| !r.enabled)
            .map(|r| r.rule_id.clone())
            .collect();
        let removed: Vec<String> = self
            .by_rule
            .keys()
            .filter(|id| !rules.iter().any(|r| &r.rule_id == *id))
            .cloned()
            .collect();
        self.rules = rules;
        self.recompile();
        for rule_id in disabled.iter().chain(removed.iter()) {
            let ids: Vec<String> = self
                .by_rule
                .get(rule_id)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            for id in ids {
                if self.events.get(&id).is_some_and(|e| e.state.is_active()) {
                    self.recover(&id, now, "rule_disabled");
                }
            }
        }
    }

    pub fn set_silences(&mut self, silences: Vec<Silence>) {
        self.silences = silences
            .into_iter()
            .map(|s| {
                let c = Compiled::new(&s.scope);
                (s, c)
            })
            .collect();
    }

    pub fn set_channels(&mut self, channels: Vec<Channel>) {
        self.channels = channels
            .into_iter()
            .map(|c| (c.channel_id.clone(), c))
            .collect();
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.rule_id == id)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn event(&self, id: &str) -> Option<&Event> {
        self.events.get(id)
    }

    /// 所有尚未解決的事件
    pub fn open_events(&self) -> impl Iterator<Item = &Event> {
        self.events
            .values()
            .filter(|e| e.state != EventState::Resolved)
    }

    pub fn warmup(&self) -> &Warmup {
        &self.warmup
    }

    /// 最近一個 tick 的主機（靜音的「目前符合 N 台主機」）
    pub fn hosts(&self) -> Arc<Vec<HostView>> {
        self.last_hosts.clone()
    }

    // ------------------------------------------------------------------
    // 索引
    // ------------------------------------------------------------------

    fn index(&mut self, ev: &Event) {
        if ev.state == EventState::Resolved {
            return;
        }
        self.by_dedup
            .insert(ev.dedup_key.clone(), ev.event_id.clone());
        self.by_rule
            .entry(ev.rule_id.clone())
            .or_default()
            .insert(ev.event_id.clone());
        if let Some(p) = &ev.parent_id {
            self.children
                .entry(p.clone())
                .or_default()
                .insert(ev.event_id.clone());
        }
    }

    fn unindex(&mut self, ev: &Event) {
        if self.by_dedup.get(&ev.dedup_key) == Some(&ev.event_id) {
            self.by_dedup.remove(&ev.dedup_key);
        }
        if let Some(set) = self.by_rule.get_mut(&ev.rule_id) {
            set.remove(&ev.event_id);
        }
        if let Some(p) = &ev.parent_id
            && let Some(set) = self.children.get_mut(p)
        {
            set.remove(&ev.event_id);
        }
    }

    pub(super) fn touch(&mut self, id: &str, change: bool) {
        self.dirty.insert(id.to_string());
        if change && let Some(ev) = self.events.get(id) {
            let kind = if ev.state == EventState::Resolved {
                "alert.resolved"
            } else {
                "alert.updated"
            };
            self.out.changes.push(Change {
                kind,
                event: ev.summary_json(),
            });
        }
    }

    pub(super) fn timeline(
        &mut self,
        id: &str,
        at: i64,
        actor: &str,
        action: &'static str,
        details: Value,
    ) {
        self.out.timeline.push(Timeline {
            event_id: id.to_string(),
            at,
            actor: actor.to_string(),
            action,
            details,
        });
    }

    /// 刪除主機資料（`kairos/PRIVACY.md` 第 4 節）：忘記這台主機的進行中事件與判斷狀態，也丟棄尚未
    /// 寫出的輸出，之後不會再寫入這些事件（資料表中的列由刪除工作刪除）。回傳忘記的事件數
    pub fn forget_agent(&mut self, agent_id: &str) -> usize {
        let ids: Vec<String> = self
            .events
            .values()
            .filter(|e| e.agent_id.as_deref() == Some(agent_id))
            .map(|e| e.event_id.clone())
            .collect();
        for id in &ids {
            if let Some(ev) = self.events.remove(id) {
                self.unindex(&ev);
                self.children.remove(id);
            }
            self.dirty.remove(id);
        }
        let gone = |id: &str| ids.iter().any(|x| x == id);
        self.out.timeline.retain(|t| !gone(&t.event_id));
        self.out
            .deliveries
            .retain(|d| d.event_id.as_deref().is_none_or(|e| !gone(e)));
        self.out.events.retain(|e| !gone(&e.event_id));
        self.out
            .changes
            .retain(|c| c.event["event_id"].as_str().is_none_or(|e| !gone(e)));
        // 以主機為對象的去重鍵（`規則|agent_id`）上的連續成立、抖動、抑制；被取代與身分驗證失敗的紀錄
        self.streaks.retain(|k, _| !k.contains(agent_id));
        self.flaps.retain(|k, _| !k.contains(agent_id));
        self.suppressed.retain(|k, _| !k.contains(agent_id));
        self.superseded.remove(agent_id);
        self.auth_failed.remove(agent_id);
        ids.len()
    }

    /// 取出輸出（有變化的事件、處理紀錄、通知、SSE）；已解決的事件之後從記憶體移除
    pub fn take_output(&mut self) -> Output {
        let mut out = std::mem::take(&mut self.out);
        let mut dirty: Vec<String> = self.dirty.drain().collect();
        dirty.sort();
        for id in dirty {
            if let Some(ev) = self.events.get(&id) {
                out.events.push(ev.clone());
            }
        }
        let resolved: Vec<String> = self
            .events
            .values()
            .filter(|e| e.state == EventState::Resolved)
            .map(|e| e.event_id.clone())
            .collect();
        for id in resolved {
            if let Some(ev) = self.events.remove(&id) {
                self.unindex(&ev);
                self.children.remove(&id);
            }
        }
        out
    }

    // ------------------------------------------------------------------
    // 事件編號
    // ------------------------------------------------------------------

    fn next_id(&mut self, now: i64) -> String {
        let day = chrono::DateTime::from_timestamp_millis(now)
            .unwrap_or_default()
            .with_timezone(&self.cfg.offset)
            .format("%Y%m%d")
            .to_string();
        let seq = self.seq_by_day.entry(day.clone()).or_insert(0);
        *seq += 1;
        format!("E-{day}-{:04}", *seq)
    }

    // ------------------------------------------------------------------
    // 事件的生命週期
    // ------------------------------------------------------------------

    /// 開新事件；回傳事件編號
    pub(super) fn open(&mut self, rule: &Rule, subject: &str, sig: Signal, now: i64) -> String {
        let id = self.next_id(now);
        let attention = rule.attention();
        let ev = Event {
            event_id: id.clone(),
            rule_id: rule.rule_id.clone(),
            dedup_key: dedup_key(&rule.rule_id, subject),
            kind: rule.kind.clone(),
            category: rule.category,
            severity: sig.severity,
            attention,
            state: EventState::Firing,
            acked: false,
            title: sig.title,
            subject: subject.to_string(),
            agent_id: sig.agent_id,
            code: sig.code,
            parent_id: sig.parent.clone(),
            episode: sig.episode,
            details: sig.details,
            started_at: now,
            updated_at: now,
            last_signal_at: now,
            recovered_at: None,
            resolved_at: None,
            acked_at: None,
            acked_by: None,
            assignee: None,
            eta_at: None,
            root_cause: None,
            resolve_note: None,
            resolved_by: None,
            occurrences: 1,
            reopen_count: 0,
            flapping: false,
            silenced: false,
            in_maintenance: false,
            snooze_until: None,
            notify: NotifyState::default(),
        };
        self.index(&ev);
        self.events.insert(id.clone(), ev);
        self.flap(&id, now);
        self.refresh_silence(&id, now);
        self.dirty.insert(id.clone());
        if let Some(ev) = self.events.get(&id) {
            self.out.changes.push(Change {
                kind: "alert.created",
                event: ev.summary_json(),
            });
        }
        self.timeline(
            &id,
            now,
            "system",
            "fired",
            json!({ "parent_id": sig.parent }),
        );
        // 網段事件稍後才通知（AGGREGATE_HOLD_MS，見 lifecycle）
        if attention == Attention::Action
            && sig.parent.is_none()
            && rule.kind != kind::SUBNET_OFFLINE
        {
            self.notify(&id, "firing", now, json!({}));
        }
        id
    }

    /// 條件解除：觸發中／已確認 → 已恢復（規劃提醒與紀錄直接為已解決）
    pub(super) fn recover(&mut self, id: &str, now: i64, why: &'static str) {
        let Some(ev) = self.events.get_mut(id) else {
            return;
        };
        if !ev.state.is_active() {
            return;
        }
        if ev.attention != Attention::Action {
            ev.state = EventState::Resolved;
            ev.recovered_at = Some(now);
            ev.resolved_at = Some(now);
            ev.root_cause = Some(RootCause::ConditionCleared);
            ev.updated_at = now;
            let id = id.to_string();
            self.timeline(
                &id,
                now,
                "system",
                "resolved",
                json!({"root_cause": "condition_cleared"}),
            );
            self.touch(&id, true);
            return;
        }
        ev.state = EventState::Recovered;
        ev.recovered_at = Some(now);
        ev.updated_at = now;
        let id = id.to_string();
        self.flap(&id, now);
        let ev = &self.events[&id];
        let notify_recovery = ev.notify.notified && ev.parent_id.is_none() && !ev.flapping;
        self.timeline(&id, now, "system", "recovered", json!({ "why": why }));
        self.touch(&id, true);
        if notify_recovery
            && let Some(rule) = self.rule_of(&id)
            && rule
                .notify
                .effective(self.events[&id].severity)
                .notify_recovery
        {
            self.notify(&id, "recovery", now, json!({}));
        }
        self.after_parent_change(&id, now);
    }

    /// 已恢復的事件在 10 分鐘內再次觸發：回到觸發中
    pub(super) fn reopen(&mut self, id: &str, now: i64) {
        let Some(ev) = self.events.get_mut(id) else {
            return;
        };
        ev.state = if ev.acked {
            EventState::Acknowledged
        } else {
            EventState::Firing
        };
        ev.recovered_at = None;
        ev.reopen_count += 1;
        ev.occurrences += 1;
        ev.updated_at = now;
        ev.last_signal_at = now;
        let count = ev.reopen_count;
        let notify = !ev.acked && ev.parent_id.is_none() && ev.attention == Attention::Action;
        let id = id.to_string();
        self.flap(&id, now);
        self.timeline(
            &id,
            now,
            "system",
            "reopened",
            json!({ "count": count + 1 }),
        );
        self.touch(&id, true);
        if notify && !self.events[&id].flapping {
            self.notify(&id, "reopen", now, json!({ "occurrence": count + 1 }));
        }
    }

    /// 結案（使用者或系統）
    pub(super) fn close(
        &mut self,
        id: &str,
        now: i64,
        cause: RootCause,
        actor: &str,
        note: Option<String>,
    ) {
        let Some(ev) = self.events.get_mut(id) else {
            return;
        };
        if ev.state == EventState::Resolved {
            return;
        }
        if ev.state.is_active()
            && let Some(ep) = ev.episode
        {
            // 條件仍成立時手動解決：同一段異常不再重新開啟
            self.suppressed.insert(ev.dedup_key.clone(), ep);
        }
        let ev = self.events.get_mut(id).expect("事件存在");
        ev.state = EventState::Resolved;
        ev.resolved_at = Some(now);
        ev.recovered_at.get_or_insert(now);
        ev.root_cause = Some(cause);
        ev.resolve_note = note.clone();
        ev.resolved_by = Some(actor.to_string());
        ev.updated_at = now;
        let id = id.to_string();
        self.timeline(
            &id,
            now,
            actor,
            "resolved",
            json!({ "root_cause": cause.as_str(), "note": note }),
        );
        self.touch(&id, true);
        self.detach_children(&id, now, true);
        self.after_parent_change(&id, now);
    }

    /// 父事件恢復或結案時：仍未恢復的子事件脫離成為獨立事件，以一則彙整通知（§3.3 C）；
    /// `closing`（父事件結案）時，已恢復的子事件一併結案
    pub(super) fn detach_children(&mut self, parent: &str, now: i64, closing: bool) {
        let Some(kids) = self.children.get(parent).cloned() else {
            return;
        };
        for kid in kids {
            let Some(ev) = self.events.get_mut(&kid) else {
                continue;
            };
            if ev.state == EventState::Resolved {
                continue;
            }
            if ev.state.is_active() {
                ev.parent_id = None;
                ev.updated_at = now;
                if let Some(set) = self.children.get_mut(parent) {
                    set.remove(&kid);
                }
                self.timeline(
                    &kid,
                    now,
                    "system",
                    "detached",
                    json!({ "parent_id": parent }),
                );
                self.touch(&kid, true);
                // 子事件可能本身也是彙總事件（網段事件在大規模離線之下）
                let group = format!("detach:{parent}");
                self.notify_grouped(
                    &kid,
                    "detached",
                    now,
                    json!({ "parent_id": parent }),
                    Some(group),
                );
            } else if closing {
                // 已恢復的子事件隨父事件結案
                self.close(&kid, now, RootCause::SelfRecovered, "system", None);
            }
        }
    }

    /// 子事件狀態改變後重新判斷父事件是否恢復
    fn after_parent_change(&mut self, id: &str, now: i64) {
        let Some(parent) = self.events.get(id).and_then(|e| e.parent_id.clone()) else {
            return;
        };
        self.check_parent(&parent, now);
    }

    /// 彙總事件：子事件 90% 以上恢復時轉為已恢復（只有子事件時才判斷）
    pub(super) fn check_parent(&mut self, parent: &str, now: i64) {
        let Some(p) = self.events.get(parent) else {
            return;
        };
        if !p.state.is_active() || p.category != Category::Aggregate {
            return;
        }
        let (total, recovered) = self.member_counts(parent);
        if total == 0 {
            return;
        }
        if recovered as f64 / total as f64 >= PARENT_RECOVERY_RATIO {
            self.set_detail(parent, "recovered_hosts", json!(recovered));
            self.recover(parent, now, "children_recovered");
            self.detach_children(parent, now, false);
        }
    }

    /// 彙總事件涵蓋的主機數與其中已恢復的數量（子事件中的彙總事件展開到主機層）
    pub fn member_counts(&self, parent: &str) -> (usize, usize) {
        let mut total = 0;
        let mut recovered = 0;
        let mut stack = vec![parent.to_string()];
        let mut seen = HashSet::new();
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            for kid in self.children.get(&p).into_iter().flatten() {
                let Some(ev) = self.events.get(kid) else {
                    continue;
                };
                if ev.category == Category::Aggregate {
                    stack.push(kid.clone());
                } else {
                    total += 1;
                    if !ev.state.is_active() {
                        recovered += 1;
                    }
                }
            }
        }
        (total, recovered)
    }

    pub(super) fn set_detail(&mut self, id: &str, key: &str, value: Value) {
        if let Some(ev) = self.events.get_mut(id)
            && let Value::Object(map) = &mut ev.details
        {
            map.insert(key.to_string(), value);
            self.dirty.insert(id.to_string());
        }
    }

    /// 把事件掛到父事件之下（原本獨立的事件被吸收）
    pub(super) fn adopt(&mut self, parent: &str, child: &str, now: i64) {
        let Some(ev) = self.events.get_mut(child) else {
            return;
        };
        if ev.parent_id.as_deref() == Some(parent) || ev.event_id == parent {
            return;
        }
        if let Some(old) = ev.parent_id.replace(parent.to_string())
            && let Some(set) = self.children.get_mut(&old)
        {
            set.remove(child);
        }
        ev.updated_at = now;
        self.children
            .entry(parent.to_string())
            .or_default()
            .insert(child.to_string());
        self.timeline(
            child,
            now,
            "system",
            "absorbed",
            json!({ "parent_id": parent }),
        );
        self.touch(child, true);
    }

    /// 狀態轉換的時間（抖動偵測）
    fn flap(&mut self, id: &str, now: i64) {
        let Some(key) = self.events.get(id).map(|e| e.dedup_key.clone()) else {
            return;
        };
        let q = self.flaps.entry(key).or_default();
        q.push_back(now);
        while q.front().is_some_and(|t| now - t > FLAP_WINDOW_MS) {
            q.pop_front();
        }
        if q.len() >= FLAP_TRANSITIONS
            && let Some(ev) = self.events.get_mut(id)
            && !ev.flapping
        {
            ev.flapping = true;
            let id = id.to_string();
            self.timeline(
                &id,
                now,
                "system",
                "flapping",
                json!({ "transitions": FLAP_TRANSITIONS }),
            );
            self.touch(&id, true);
        }
    }

    pub(super) fn rule_of(&self, event_id: &str) -> Option<Rule> {
        let rule_id = &self.events.get(event_id)?.rule_id;
        self.rules.iter().find(|r| &r.rule_id == rule_id).cloned()
    }

    // ------------------------------------------------------------------
    // 靜音
    // ------------------------------------------------------------------

    /// 事件符合的靜音（`None`：沒有）；回傳（是否靜音, 是否在維護時段內）
    pub(super) fn silence_match(&self, ev: &Event, now: i64) -> (bool, bool, Vec<String>) {
        // 不可靜音的系統規則由程式碼決定（rules::is_unsilenceable），不只看資料庫那一列的 params
        let unsilenceable = super::rules::is_unsilenceable(&ev.rule_id)
            || self
                .rules
                .iter()
                .find(|r| r.rule_id == ev.rule_id)
                .is_some_and(|r| {
                    r.params.get("unsilenceable").and_then(Value::as_bool) == Some(true)
                });
        if unsilenceable || !self.silences.iter().any(|(s, _)| s.is_active(now)) {
            return (false, false, Vec::new());
        }
        let host = ev
            .agent_id
            .as_deref()
            .and_then(|a| self.host_index.get(a))
            .and_then(|i| self.last_hosts.get(*i));
        let subnet: Option<Cidr> = (ev.kind == kind::SUBNET_OFFLINE)
            .then(|| ev.subject.parse().ok())
            .flatten();
        let mut silenced = false;
        let mut maintenance = false;
        let mut matched = Vec::new();
        for (s, scope) in &self.silences {
            if !s.is_active(now) {
                continue;
            }
            let rule_listed = s.rules.iter().any(|r| r == &ev.rule_id);
            if !s.rules.is_empty() && !rule_listed {
                continue;
            }
            let hit = match ev.category {
                // 系統事件：只有臨時靜音、且在「規則」明確選取時（§4.2）；維護時段不抑制系統事件
                Category::System => s.kind == SilenceKind::Silence && rule_listed,
                Category::Aggregate => {
                    scope.is_all() || subnet.is_some_and(|c| scope.covers_subnet(&c))
                }
                Category::Host | Category::Identity => match host {
                    Some(h) => scope.matches(&h.facts()),
                    None => scope.is_all(),
                },
            };
            if hit {
                silenced = true;
                if s.kind == SilenceKind::Maintenance {
                    maintenance = true;
                }
                matched.push(s.silence_id.clone());
            }
        }
        (silenced, maintenance, matched)
    }

    fn refresh_silence(&mut self, id: &str, now: i64) {
        let Some(ev) = self.events.get(id) else {
            return;
        };
        let (silenced, maintenance, _) = self.silence_match(ev, now);
        let ev = self.events.get_mut(id).expect("事件存在");
        if ev.silenced != silenced || ev.in_maintenance != maintenance {
            ev.silenced = silenced;
            ev.in_maintenance = maintenance;
            let id = id.to_string();
            self.touch(&id, true);
        }
    }

    // ------------------------------------------------------------------
    // 通知
    // ------------------------------------------------------------------

    /// 事件要送的管道（依規則；升級通知送升級對象；通知管道失敗事件改送其他所有健康的管道，§5.4）
    pub(super) fn route(&self, ev: &Event, rule: &Rule, escalation: bool) -> Vec<String> {
        let candidates: Vec<String> = if rule.rule_id == ids::CHANNEL_FAILED {
            let mut all: Vec<String> = self
                .channels
                .values()
                .filter(|c| c.channel_id != ev.subject && !c.failed())
                .map(|c| c.channel_id.clone())
                .collect();
            all.sort();
            all
        } else if escalation {
            rule.notify.escalate_channels.clone()
        } else {
            rule.channels.clone()
        };
        candidates
            .into_iter()
            .filter(|id| {
                self.channels
                    .get(id)
                    .is_some_and(|c| c.enabled && (escalation || c.accepts(ev.severity)))
            })
            .collect()
    }

    pub(super) fn notify(&mut self, id: &str, kind_name: &'static str, now: i64, extra: Value) {
        self.notify_grouped(id, kind_name, now, extra, None);
    }

    /// 依事件的狀態排入通知（靜音、暫停通知、規劃提醒與紀錄、子事件都不送）
    pub(super) fn notify_grouped(
        &mut self,
        id: &str,
        kind_name: &'static str,
        now: i64,
        extra: Value,
        group: Option<String>,
    ) {
        let Some(ev) = self.events.get(id) else {
            return;
        };
        if ev.attention != Attention::Action || ev.parent_id.is_some() {
            return;
        }
        self.refresh_silence(id, now);
        let ev = &self.events[id];
        if ev.silenced && kind_name != "ack" {
            return;
        }
        let Some(rule) = self.rule_of(id) else {
            return;
        };
        let escalation = kind_name == "escalation";
        let channels = self.route(ev, &rule, escalation);
        if channels.is_empty() {
            // 沒有管道時只在主控台顯示；仍記下「已通知」的狀態，讓恢復、確認的判斷一致
            if matches!(kind_name, "firing" | "reopen" | "detached") {
                let ev = self.events.get_mut(id).expect("事件存在");
                ev.notify.notified = true;
                ev.notify.last_sent_at = Some(now);
                ev.notify.notified_rank.get_or_insert(ev.severity.rank());
                self.dirty.insert(id.to_string());
            }
            return;
        }
        let payload = self.payload(ev, &rule, kind_name, extra);
        let urgent = kind_name == "firing" && ev.severity == Severity::Emergency;
        let sev_rank = ev.severity.rank();
        for c in &channels {
            // 管道可以關閉確認通知（S11）
            if kind_name == "ack"
                && self
                    .channels
                    .get(c)
                    .and_then(|ch| ch.config.get("notify_ack"))
                    .and_then(Value::as_bool)
                    == Some(false)
            {
                continue;
            }
            self.out.deliveries.push(NewDelivery {
                event_id: Some(id.to_string()),
                channel_id: c.clone(),
                kind: kind_name,
                urgent,
                payload: payload.clone(),
                group: group.clone(),
                created_at: now,
            });
        }
        let ev = self.events.get_mut(id).expect("事件存在");
        match kind_name {
            "firing" | "reopen" | "detached" | "repeat" | "update" => {
                ev.notify.notified = true;
                ev.notify.sends += 1;
                ev.notify.last_sent_at = Some(now);
                let r = ev
                    .notify
                    .notified_rank
                    .map_or(sev_rank, |r| r.min(sev_rank));
                ev.notify.notified_rank = Some(r);
            }
            "escalation" => ev.notify.escalated = true,
            "flapping" => ev.notify.flap_summary_at = Some(now),
            _ => {}
        }
        let names: Vec<String> = channels
            .iter()
            .map(|c| self.channels.get(c).map_or(c.clone(), |ch| ch.name.clone()))
            .collect();
        let id = id.to_string();
        self.dirty.insert(id.clone());
        self.timeline(
            &id,
            now,
            "system",
            "notified",
            json!({ "kind": kind_name, "channels": names }),
        );
    }

    /// 通知的內容（不含機密；各管道的格式由 `notify::render` 產生）
    pub(super) fn payload(&self, ev: &Event, rule: &Rule, kind_name: &str, extra: Value) -> Value {
        let (total, recovered) = self.member_counts(&ev.event_id);
        let mut sample = Vec::new();
        if total > 0 {
            let mut stack = vec![ev.event_id.clone()];
            while let Some(p) = stack.pop() {
                for kid in self.children.get(&p).into_iter().flatten() {
                    let Some(k) = self.events.get(kid) else {
                        continue;
                    };
                    if k.category == Category::Aggregate {
                        stack.push(kid.clone());
                    } else if sample.len() < MAX_LISTED && k.state.is_active() {
                        sample.push(json!({
                            "agent_id": k.agent_id,
                            "hostname": k.details.get("hostname"),
                            "ip": k.details.get("ip"),
                            "offline_reason": k.details.get("offline_reason"),
                        }));
                    }
                }
            }
        }
        json!({
            "kind": kind_name,
            "event": {
                "event_id": ev.event_id,
                "rule_id": ev.rule_id,
                "rule_name": rule.name,
                "kind": ev.kind,
                "category": ev.category,
                "severity": ev.severity,
                "attention": ev.attention,
                "state": ev.state,
                "title": ev.title,
                "subject": ev.subject,
                "agent_id": ev.agent_id,
                "code": ev.code,
                "parent_id": ev.parent_id,
                "started_at": ev.started_at,
                "updated_at": ev.updated_at,
                "recovered_at": ev.recovered_at,
                "acked_by": ev.acked_by,
                "assignee": ev.assignee,
                "eta_at": ev.eta_at,
                "occurrences": ev.occurrences,
                "reopen_count": ev.reopen_count,
                "details": ev.details,
            },
            "hint": rule.description,
            "repeat": ev.notify.sends,
            "scope": { "count": total.saturating_sub(recovered), "total": total, "sample": sample },
            "notify": {
                "repeat_interval_secs": rule.notify.effective(ev.severity).repeat_interval_secs,
                "repeat_max": rule.notify.effective(ev.severity).repeat_max,
                "escalate_after_secs": rule.notify.effective(ev.severity).escalate_after_secs,
            },
            "link": self.link(&ev.event_id),
            "extra": extra,
        })
    }

    pub(super) fn link(&self, event_id: &str) -> Option<String> {
        let base = self.cfg.console_url.trim().trim_end_matches('/');
        (!base.is_empty()).then(|| format!("{base}/alerts/events/{event_id}"))
    }

    // ------------------------------------------------------------------
    // 匯流排
    // ------------------------------------------------------------------

    /// 匯流排的事件（被取代、身分驗證失敗、除役、TLS 重載、狀態表淘汰）
    pub fn on_bus(&mut self, e: &BusEvent) {
        let at = e.at.timestamp_millis();
        match &e.kind {
            EventKind::HostSuperseded { agent_id, ip, .. } => {
                let q = self.superseded.entry(agent_id.to_string()).or_default();
                q.push_back((at, *ip));
                while q.len() > 1000 {
                    q.pop_front();
                }
            }
            EventKind::AuthFailed { agent_id, ip } => {
                if self.auth_failed.len() < 10_000 || self.auth_failed.contains_key(agent_id) {
                    self.auth_failed
                        .insert(agent_id.to_ascii_lowercase(), (at, *ip));
                }
            }
            EventKind::HostDecommissioned {
                agent_id,
                ip,
                initiated_by,
                already,
            } => {
                if !*already {
                    self.on_decommissioned(agent_id, *ip, initiated_by.as_deref(), at);
                }
            }
            EventKind::HostRebooted {
                agent_id,
                hostname,
                uptime_secs,
                previous_uptime_secs,
                boot_time,
            } => {
                self.on_rebooted(
                    agent_id,
                    hostname,
                    *uptime_secs,
                    *previous_uptime_secs,
                    boot_time.as_deref(),
                    at,
                );
            }
            EventKind::RegistryEvicted { .. } => {
                self.evictions.push_back(at);
                while self.evictions.len() > 10_000 {
                    self.evictions.pop_front();
                }
            }
            EventKind::TlsReload { ok, crl, message } => {
                if *ok {
                    self.tls_failed = None;
                } else if self.tls_failed.is_none() {
                    self.tls_failed = Some((at, *crl, message.clone().unwrap_or_default()));
                }
            }
            EventKind::RemoteCondition {
                rule,
                subject,
                active,
                title,
                details,
            } => {
                let key = (*rule, subject.clone());
                if *active {
                    let since = self.remote_conditions.get(&key).map_or(at, |c| c.0);
                    self.remote_conditions
                        .insert(key, (since, title.clone(), details.clone()));
                } else {
                    self.remote_conditions.remove(&key);
                }
            }
            EventKind::UpgradeWindow {
                agent_id,
                rollout_id,
                until_ms,
            } => match until_ms {
                Some(until) => {
                    self.upgrading
                        .insert(agent_id.clone(), (rollout_id.clone(), *until));
                }
                None => {
                    // 期限前解除＝主機已回報結果（連上過）：之後的離線與這次升級無關，不再註明。
                    // 期限已過的解除保留註記（仍離線的主機這一輪或下一輪才開事件）
                    if let Some((_, until)) = self.upgrading.remove(agent_id)
                        && until > at
                    {
                        self.upgrade_notes.remove(agent_id);
                    }
                }
            },
            EventKind::AuditIntegrity {
                ok,
                reason,
                id,
                summary,
            } => {
                if *ok {
                    self.audit_failed = None;
                } else {
                    let since = self.audit_failed.as_ref().map_or(at, |f| f.0);
                    self.audit_failed = Some((
                        since,
                        reason.clone().unwrap_or_default(),
                        *id,
                        summary.clone(),
                    ));
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // 使用者的動作
    // ------------------------------------------------------------------

    fn actionable(&self, id: &str) -> Result<&Event, ActionError> {
        let ev = self.events.get(id).ok_or_else(|| {
            action_error(
                "event_not_found",
                format!("沒有進行中的事件 {id}（可能已解決）"),
            )
        })?;
        if ev.state == EventState::Resolved {
            return Err(action_error("invalid_state", "事件已解決，只能新增註解"));
        }
        Ok(ev)
    }

    /// 確認（§5.2）：停止重送與升級；`assign_to_me` 時同時設定負責人
    pub fn ack(
        &mut self,
        id: &str,
        actor: &str,
        assign: bool,
        eta_minutes: Option<u32>,
        now: i64,
    ) -> Result<bool, ActionError> {
        let ev = self.actionable(id)?;
        if ev.attention != Attention::Action {
            return Err(action_error(
                "not_acknowledgeable",
                if ev.attention == Attention::Advisory {
                    "規劃提醒：不需要確認，條件解除時自動結束"
                } else {
                    "紀錄：不需要確認"
                },
            ));
        }
        if ev.acked {
            return Ok(false);
        }
        let notified = ev.notify.notified;
        let ev = self.events.get_mut(id).expect("事件存在");
        ev.acked = true;
        ev.acked_at = Some(now);
        ev.acked_by = Some(actor.to_string());
        if ev.state == EventState::Firing {
            ev.state = EventState::Acknowledged;
        }
        if assign {
            ev.assignee = Some(actor.to_string());
        }
        ev.eta_at = eta_minutes.map(|m| now + i64::from(m) * 60_000);
        ev.updated_at = now;
        let id = id.to_string();
        self.timeline(
            &id,
            now,
            actor,
            "acknowledged",
            json!({ "assign": assign, "eta_minutes": eta_minutes }),
        );
        self.touch(&id, true);
        if notified {
            self.notify(
                &id,
                "ack",
                now,
                json!({ "by": actor, "eta_minutes": eta_minutes }),
            );
        }
        Ok(true)
    }

    pub fn assign(
        &mut self,
        id: &str,
        actor: &str,
        assignee: &str,
        now: i64,
    ) -> Result<(), ActionError> {
        self.actionable(id)?;
        let a = assignee.trim();
        if a.is_empty() || a.chars().count() > 100 || a.chars().any(char::is_control) {
            return Err(action_error(
                "invalid_parameter",
                "負責人必須是 1～100 個字元",
            ));
        }
        let ev = self.events.get_mut(id).expect("事件存在");
        ev.assignee = Some(a.to_string());
        ev.updated_at = now;
        let id = id.to_string();
        self.timeline(&id, now, actor, "assigned", json!({ "assignee": a }));
        self.touch(&id, true);
        Ok(())
    }

    /// 暫停通知（只停止這個事件的重送；狀態不變）
    pub fn snooze(
        &mut self,
        id: &str,
        actor: &str,
        hours: u32,
        now: i64,
    ) -> Result<(), ActionError> {
        let ev = self.actionable(id)?;
        if ev.attention != Attention::Action {
            return Err(action_error(
                "not_acknowledgeable",
                "規劃提醒與紀錄不推播，不需要暫停通知",
            ));
        }
        if !(1..=24).contains(&hours) {
            return Err(action_error(
                "invalid_parameter",
                "暫停通知的時數必須是 1～24（S07 提供 1／4／8 小時）",
            ));
        }
        let ev = self.events.get_mut(id).expect("事件存在");
        ev.snooze_until = Some(now + i64::from(hours) * 3_600_000);
        ev.updated_at = now;
        let id = id.to_string();
        self.timeline(&id, now, actor, "snoozed", json!({ "hours": hours }));
        self.touch(&id, true);
        Ok(())
    }

    /// 解決（必選根因類別；有未恢復的子事件時它們脫離成為獨立事件）
    pub fn resolve(
        &mut self,
        id: &str,
        actor: &str,
        cause: RootCause,
        note: Option<String>,
        now: i64,
    ) -> Result<usize, ActionError> {
        let ev = self.actionable(id)?;
        if ev.attention != Attention::Action {
            return Err(action_error(
                "not_acknowledgeable",
                "規劃提醒與紀錄在條件解除時自動結束，不提供「解決」",
            ));
        }
        let detached = self
            .children
            .get(id)
            .map(|k| {
                k.iter()
                    .filter(|c| self.events.get(*c).is_some_and(|e| e.state.is_active()))
                    .count()
            })
            .unwrap_or(0);
        self.close(id, now, cause, actor, note);
        Ok(detached)
    }

    pub fn comment(
        &mut self,
        id: &str,
        actor: &str,
        text: &str,
        now: i64,
    ) -> Result<(), ActionError> {
        let t = text.trim();
        if t.is_empty() || t.chars().count() > 2000 {
            return Err(action_error(
                "invalid_parameter",
                "註解必須是 1～2000 個字元",
            ));
        }
        if !self.events.contains_key(id) {
            return Err(action_error(
                "event_not_found",
                format!("沒有進行中的事件 {id}"),
            ));
        }
        let id = id.to_string();
        self.timeline(&id, now, actor, "commented", json!({ "text": t }));
        self.dirty.insert(id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // 啟動時的系統事件
    // ------------------------------------------------------------------

    /// 叢集模式：啟動前設定「這次是從另一個節點接手」（[`Engine::record_startup`] 據此改寫事件）
    pub fn set_handover(&mut self, handover: Option<Handover>) {
        self.handover = handover;
    }

    /// Kairos 啟動：記錄「已重新啟動」（上次沒有正常關機時為嚴重）與停機期間（§3.3 B 第 4 點）。
    ///
    /// 叢集模式下從另一個節點接手時（`kairos/HA.md` 第 13.2 節）：「已重新啟動」改為「叢集的 leader 已由
    /// A 切換到 B」（`KRS-I8501`；上一個 leader 沒有正常交還時為警告），不開「停機」；上一個 leader 的心跳
    /// 太舊時「停機 N 分鐘」改為「叢集沒有 leader 達 N 分鐘」（`KRS-E8503`）
    pub fn record_startup(
        &mut self,
        previous: Option<(i64, bool)>,
        enroll_closed_default: bool,
        in_maintenance: bool,
        now: i64,
    ) {
        let handover = self.handover.clone();
        if let Some(rule) = self.rule(ids::RESTARTED).filter(|r| r.enabled).cloned() {
            let unclean = previous.is_some_and(|(_, clean)| !clean);
            let (severity, title, code) = match (&handover, unclean) {
                (Some(h), unclean) => (
                    if unclean {
                        Severity::Warning
                    } else {
                        Severity::Info
                    },
                    format!(
                        "叢集的 leader 已由 {} 切換到 {}{}",
                        h.from,
                        h.to,
                        if unclean {
                            "（上一個 leader 沒有正常交還）"
                        } else {
                            ""
                        }
                    ),
                    Some(crate::cluster::CODE_LEADER_ACQUIRED.to_string()),
                ),
                (None, true) => (
                    Severity::Critical,
                    "Kairos 已重新啟動（上次沒有正常關機）".to_string(),
                    None,
                ),
                (None, false) => (Severity::Info, "Kairos 已重新啟動".to_string(), None),
            };
            let mut r = rule.clone();
            r.severity = severity;
            let id = self.open(
                &r,
                &format!("start:{now}"),
                Signal {
                    severity,
                    title,
                    code,
                    agent_id: None,
                    details: json!({
                        "previous_heartbeat_at": previous.map(|p| p.0),
                        "clean_shutdown": previous.map(|p| p.1),
                        "version": crate::VERSION,
                        "previous_leader": handover.as_ref().map(|h| h.from.clone()),
                        "leader": handover.as_ref().map(|h| h.to.clone()),
                    }),
                    episode: None,
                    parent: None,
                },
                now,
            );
            if severity == Severity::Info {
                self.close(&id, now, RootCause::ConditionCleared, "system", None);
            } else {
                self.recover(&id, now, "one_shot");
            }
        }
        if let Some((last, _)) = previous
            && let Some(rule) = self.rule(ids::DOWNTIME).filter(|r| r.enabled).cloned()
        {
            let down = now - last;
            let min = rule.param_u64("min_secs", 300) as i64 * 1000;
            // 正常的 leader 換手（上一個 leader 的心跳還很新）不算停機
            let handed_over = handover.as_ref().is_some_and(|h| h.recent);
            if down > min && !in_maintenance && !handed_over {
                let minutes = down / 60_000;
                let (title, code) = match &handover {
                    Some(_) => (
                        format!("叢集沒有 leader 達 {minutes} 分鐘，期間資料未收集"),
                        Some(crate::cluster::CODE_COORDINATION.to_string()),
                    ),
                    None => (format!("Kairos 停機 {minutes} 分鐘，期間資料未收集"), None),
                };
                let id = self.open(
                    &rule,
                    &format!("down:{last}"),
                    Signal {
                        severity: rule.severity,
                        title,
                        code,
                        agent_id: None,
                        details: json!({ "from": last, "to": now, "minutes": minutes }),
                        episode: Some(last),
                        parent: None,
                    },
                    now,
                );
                // 已經過去的停機：通知後直接為已恢復（需要人確認）
                self.recover(&id, now, "one_shot");
            }
        }
        if enroll_closed_default
            && let Some(rule) = self
                .rule(ids::ENROLL_CLOSED_DEFAULT)
                .filter(|r| r.enabled)
                .cloned()
        {
            let id = self.open(
                &rule,
                &format!("start:{now}"),
                Signal {
                    severity: rule.severity,
                    title: "註冊端點維持關閉：設定檔沒有 [enroll] 的 mode".to_string(),
                    code: Some("KRS-I2010".to_string()),
                    agent_id: None,
                    details: json!({}),
                    episode: None,
                    parent: None,
                },
                now,
            );
            self.close(&id, now, RootCause::ConditionCleared, "system", None);
        }
    }

    /// 主機自行除役：每台一則紀錄；之前觸發、未恢復的主機事件自動結束（§2.2）
    /// 主機重新開機（`sys.host_rebooted`；D5）：資訊等級，建立後立即結案（只留紀錄，不通知）
    fn on_rebooted(
        &mut self,
        agent_id: &str,
        hostname: &str,
        uptime_secs: u64,
        previous_uptime_secs: u64,
        boot_time: Option<&str>,
        at: i64,
    ) {
        let Some(rule) = self.rule(ids::HOST_REBOOTED).filter(|r| r.enabled).cloned() else {
            return;
        };
        let host = self.last_hosts.iter().find(|h| &*h.agent_id == agent_id);
        if host.is_some_and(|h| h.decommissioned) {
            return;
        }
        let name = host
            .map(|h| h.name().to_string())
            .unwrap_or_else(|| hostname.to_string());
        let ip = host.and_then(|h| h.ip).map(|i| i.to_string());
        let id = self.open(
            &rule,
            agent_id,
            Signal {
                severity: rule.severity,
                title: format!("{name} 已重新開機"),
                code: None,
                agent_id: Some(agent_id.to_string()),
                details: json!({
                    "hostname": name,
                    "ip": ip,
                    "uptime_secs": uptime_secs,
                    "previous_uptime_secs": previous_uptime_secs,
                    "boot_time": boot_time,
                    "note": "這段期間的磁碟讀寫與網路速率沒有資料（計數器重置）",
                }),
                episode: Some(at),
                parent: None,
            },
            at,
        );
        self.close(&id, at, RootCause::ConditionCleared, "system", None);
    }

    fn on_decommissioned(
        &mut self,
        agent_id: &str,
        ip: Option<IpAddr>,
        initiated_by: Option<&str>,
        at: i64,
    ) {
        self.decommissions.push_back((at, agent_id.to_string()));
        while self.decommissions.len() > 100_000 {
            self.decommissions.pop_front();
        }
        let open: Vec<String> = self
            .events
            .values()
            .filter(|e| e.agent_id.as_deref() == Some(agent_id) && e.state != EventState::Resolved)
            .map(|e| e.event_id.clone())
            .collect();
        for id in open {
            self.close(&id, at, RootCause::Decommissioned, "system", None);
        }
        if let Some(rule) = self
            .rule(ids::HOST_DECOMMISSIONED)
            .filter(|r| r.enabled)
            .cloned()
        {
            let hostname = self
                .last_hosts
                .iter()
                .find(|h| &*h.agent_id == agent_id)
                .map(|h| h.name().to_string())
                .unwrap_or_else(|| agent_id.to_string());
            let id = self.open(
                &rule,
                agent_id,
                Signal {
                    severity: rule.severity,
                    title: format!("{hostname} 已自行除役（完全移除）"),
                    code: None,
                    agent_id: Some(agent_id.to_string()),
                    details: json!({
                        "hostname": hostname,
                        "ip": ip.map(|i| i.to_string()),
                        "initiated_by": initiated_by,
                    }),
                    episode: Some(at),
                    parent: None,
                },
                at,
            );
            self.close(&id, at, RootCause::ConditionCleared, "system", None);
        }
    }
}
