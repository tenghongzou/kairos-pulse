//! 集中設定的 Kairos 端（REMOTE.md 第 3.4 節）：設定檔的檢查、指派的選擇器、合併成每台主機的期望設定、
//! 版本與雜湊、相同優先序的衝突偵測，以及主控台表單用的白名單（`GET /api/v1/config/schema`）。
//!
//! - 合併：比對到的指派依優先序**由低到高**套用（[`kairos_proto::mgmt::settings::overlay`]），同一個鍵以
//!   優先序高者為準；相同優先序的兩個指派設定同一個鍵且比對到同一台主機時，**儲存時**拒絕
//!   （409 `assignment_conflict`，[`find_conflicts`]）。主機的標籤或群組之後才改變而造成的重疊，以指派的
//!   編號決定（後建立的為準），不會無法計算。
//! - 版本：任何設定檔、指派改變時 `meta.config_rev` 加 1；只有期望設定的雜湊改變的主機才把 `desired_rev`
//!   更新為當時的 `config_rev`（[`recompute`]）。沒有任何設定時 `desired_rev = 0`。

use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use kairos_proto::mgmt::settings::{self as ws, Apply, Kind, Privacy};
use kairos_proto::mgmt::{KeyError, settings_hash};

use super::store::{AssignmentRow, ConfigTarget, DesiredUpdate, ProfileRow};
use crate::store::db::{Store, StoreResult};

/// 設定檔名稱的長度上限（字元）
pub const MAX_PROFILE_NAME_CHARS: usize = 128;
/// 設定檔說明的長度上限（字元）
pub const MAX_PROFILE_DESCRIPTION_CHARS: usize = 1000;
/// 設定檔 `settings` 正規化後的上限（bytes；與協定的單行上限相同）
pub const MAX_PROFILE_SETTINGS_BYTES: usize = 65_536;
/// 變更原因的長度上限（字元）
pub const MAX_REASON_CHARS: usize = 500;
/// 影響超過這個台數的設定變更需要 `impact_token`（REMOTE.md 第 8.1.1 節；主控台 L1 確認）
pub const IMPACT_CONFIRM_HOSTS: usize = 1000;
/// 指派的數量上限（避免意外的大量指派讓每次重新計算變慢）
pub const MAX_ASSIGNMENTS: usize = 10_000;
/// 設定檔的數量上限
pub const MAX_PROFILES: usize = 1000;

/// 指派的選擇器（REMOTE.md 第 3.4.2 節）
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Selector {
    /// 所有主機（最多一筆）
    Global,
    /// `hosts.host_group`（不分大小寫，保存小寫）
    Group(String),
    /// hello 的 `tags`（區分大小寫）
    Tag(String),
    /// 單台（小寫的 agent_id）
    Host(String),
}

impl Selector {
    /// 解析 `global`、`group:<主機群組>`、`tag:<標籤>`、`host:<agent_id>`
    pub fn parse(text: &str) -> Result<Selector, String> {
        let t = text.trim();
        if t == "global" {
            return Ok(Selector::Global);
        }
        let (kind, value) = t.split_once(':').ok_or_else(|| {
            format!("選擇器「{t}」無效：global、group:<主機群組>、tag:<標籤>、host:<agent_id>")
        })?;
        let value = value.trim();
        let visible = |v: &str, max: usize| {
            !v.is_empty()
                && v.len() <= max
                && !v.chars().any(kairos_proto::is_forbidden_display_char)
        };
        match kind {
            "group"
                if visible(value, 256) && value.chars().count() <= crate::rbac::MAX_GROUP_CHARS =>
            {
                Ok(Selector::Group(value.to_lowercase()))
            }
            "group" => Err(format!(
                "主機群組名稱必須是 1～{} 個字元、不含控制字元",
                crate::rbac::MAX_GROUP_CHARS
            )),
            "tag" if visible(value, kairos_proto::MAX_TAG_BYTES) => {
                Ok(Selector::Tag(value.to_string()))
            }
            "tag" => Err(format!(
                "標籤必須是 1～{} bytes、不含控制字元",
                kairos_proto::MAX_TAG_BYTES
            )),
            "host" if kairos_proto::is_uuid_text(value) => {
                Ok(Selector::Host(value.to_ascii_lowercase()))
            }
            "host" => Err("host: 之後必須是主機的 agent_id（UUID）".to_string()),
            _ => Err(format!(
                "選擇器「{t}」無效：global、group:<主機群組>、tag:<標籤>、host:<agent_id>"
            )),
        }
    }

    /// 資料表的 (selector_kind, selector_value)
    pub fn to_db(&self) -> (&'static str, &str) {
        match self {
            Selector::Global => ("global", ""),
            Selector::Group(g) => ("group", g),
            Selector::Tag(t) => ("tag", t),
            Selector::Host(h) => ("host", h),
        }
    }

    pub fn from_db(kind: &str, value: &str) -> Option<Selector> {
        match kind {
            "global" => Some(Selector::Global),
            "group" => Some(Selector::Group(value.to_string())),
            "tag" => Some(Selector::Tag(value.to_string())),
            "host" => Some(Selector::Host(value.to_string())),
            _ => None,
        }
    }

    /// 文字形式（API 的 `selector`）
    pub fn text(&self) -> String {
        match self {
            Selector::Global => "global".to_string(),
            Selector::Group(g) => format!("group:{g}"),
            Selector::Tag(t) => format!("tag:{t}"),
            Selector::Host(h) => format!("host:{h}"),
        }
    }

    /// 預設的優先序（global 0、group 100、tag 200、host 1000）
    pub fn default_priority(&self) -> i64 {
        match self {
            Selector::Global => 0,
            Selector::Group(_) => 100,
            Selector::Tag(_) => 200,
            Selector::Host(_) => 1000,
        }
    }

    /// 是否比對到這台主機
    pub fn matches(&self, t: &ConfigTarget) -> bool {
        match self {
            Selector::Global => true,
            Selector::Group(g) => t
                .host_group
                .as_deref()
                .is_some_and(|h| h.trim().to_lowercase() == *g),
            Selector::Tag(tag) => t.tags.iter().any(|x| x == tag),
            Selector::Host(id) => t.agent_id == *id,
        }
    }
}

/// 設定檔的名稱：1～128 個字元、不含控制字元（前後空白去除）
pub fn check_profile_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    let count = n.chars().count();
    if count == 0
        || count > MAX_PROFILE_NAME_CHARS
        || n.chars().any(kairos_proto::is_forbidden_display_char)
    {
        return Err(format!(
            "設定檔名稱必須是 1～{MAX_PROFILE_NAME_CHARS} 個字元、不含控制字元"
        ));
    }
    Ok(n.to_string())
}

/// 設定檔的說明：最多 1000 個字元（可含換行，不含其他控制字元）
pub fn check_description(text: &str) -> Result<String, String> {
    let d = text.trim();
    if d.chars().count() > MAX_PROFILE_DESCRIPTION_CHARS
        || d.chars()
            .any(|c| c != '\n' && kairos_proto::is_forbidden_display_char(c))
    {
        return Err(format!(
            "說明最多 {MAX_PROFILE_DESCRIPTION_CHARS} 個字元、不含控制字元（可以換行）"
        ));
    }
    Ok(d.to_string())
}

/// 設定檔內容的檢查（`KRS-E0502`）：兩層物件、白名單、只能在本機設定的值、型別與範圍、鍵之間的矛盾，
/// 正規化後不超過 64 KiB。通過時回傳正規化的 JSON 文字
pub fn check_profile_settings(
    value: &Value,
) -> Result<(Map<String, Value>, String), Vec<KeyError>> {
    let err = |key: &str, reason: &str, message: String| KeyError {
        key: key.to_string(),
        reason: reason.to_string(),
        message,
    };
    let Value::Object(map) = value else {
        return Err(vec![err(
            "*",
            ws::ERR_TYPE_MISMATCH,
            "settings 必須是物件：{\"<區段>\": {\"<鍵>\": <值>}}".to_string(),
        )]);
    };
    ws::validate_profile(map)?;
    // 空的區段不保存（`{"log":{}}` 與 `{}` 相同）
    let map: Map<String, Value> = map
        .iter()
        .filter(|(_, v)| v.as_object().is_some_and(|o| !o.is_empty()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let text = kairos_proto::mgmt::canonical_json(&Value::Object(map.clone()));
    if text.len() > MAX_PROFILE_SETTINGS_BYTES {
        return Err(vec![err(
            "*",
            ws::ERR_OUT_OF_RANGE,
            format!("settings 最多 {MAX_PROFILE_SETTINGS_BYTES} bytes"),
        )]);
    }
    Ok((map, text))
}

/// 白名單的鍵（攤平；設定檔中有哪些鍵）
pub fn keys_of(settings: &Map<String, Value>) -> BTreeSet<String> {
    ws::flatten(settings)
        .map(|f| f.into_iter().map(|(k, _)| k).collect())
        .unwrap_or_default()
}

/// 解析資料表中的設定（壞掉的內容視為空，並記錄）
pub fn parse_settings(text: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => m,
        _ => {
            log::warn!("集中設定：資料表中的設定不是 JSON 物件，視為空的設定");
            Map::new()
        }
    }
}

/// 一個指派（解析後）
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub id: i64,
    pub profile_id: i64,
    pub selector: Selector,
    pub priority: i64,
    pub expires_at: Option<i64>,
}

impl Assignment {
    pub fn from_row(r: &AssignmentRow) -> Option<Assignment> {
        Some(Assignment {
            id: r.id,
            profile_id: r.profile_id,
            selector: Selector::from_db(&r.selector_kind, &r.selector_value)?,
            priority: r.priority,
            expires_at: r.expires_at,
        })
    }

    fn active(&self, now_ms: i64) -> bool {
        self.expires_at.is_none_or(|t| t > now_ms)
    }
}

/// 設定檔與指派的快照（計算期望設定）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigSnapshot {
    pub rev: i64,
    /// 設定檔 id → (名稱, settings)
    pub profiles: HashMap<i64, (String, Map<String, Value>)>,
    /// 依 (優先序, 編號) 排序
    pub assignments: Vec<Assignment>,
}

impl ConfigSnapshot {
    pub fn load(s: &dyn Store) -> StoreResult<ConfigSnapshot> {
        let rev = s.remote_config_rev()?;
        let profiles = s
            .list_profiles()?
            .into_iter()
            .map(|p: ProfileRow| (p.id, (p.name, parse_settings(&p.settings))))
            .collect();
        let mut assignments: Vec<Assignment> = s
            .list_assignments()?
            .iter()
            .filter_map(Assignment::from_row)
            .collect();
        assignments.sort_by_key(|a| (a.priority, a.id));
        Ok(ConfigSnapshot {
            rev,
            profiles,
            assignments,
        })
    }

    /// 這台主機的期望設定與每個鍵的來源（設定檔名稱）
    pub fn merged_for(
        &self,
        target: &ConfigTarget,
        now_ms: i64,
    ) -> (Map<String, Value>, BTreeMap<String, String>) {
        let mut layers: Vec<&Map<String, Value>> = Vec::new();
        let mut sources = BTreeMap::new();
        for a in &self.assignments {
            if !a.active(now_ms) || !a.selector.matches(target) {
                continue;
            }
            let Some((name, settings)) = self.profiles.get(&a.profile_id) else {
                continue;
            };
            for k in keys_of(settings) {
                sources.insert(k, name.clone());
            }
            layers.push(settings);
        }
        (ws::overlay(layers), sources)
    }
}

/// 期望設定的雜湊（空的設定也有雜湊；`desired_rev = 0` 時 `desired_hash` 為空字串）
pub fn desired_of(
    snap: &ConfigSnapshot,
    target: &ConfigTarget,
    now_ms: i64,
) -> (Map<String, Value>, BTreeMap<String, String>, String) {
    let (merged, sources) = snap.merged_for(target, now_ms);
    let hash = if merged.is_empty() {
        String::new()
    } else {
        settings_hash(&merged)
    };
    (merged, sources, hash)
}

/// 重新計算期望設定（REMOTE.md 第 3.4.4 節）：只有雜湊改變的主機才寫入，`desired_rev` 為目前的
/// `meta.config_rev`（沒有任何設定時 0）。`only` 為 `Some` 時只算這台。呼叫端在 Immediate 交易中。
/// 回傳改變的 (agent_id, desired_rev)
pub fn recompute(
    s: &dyn Store,
    snap: &ConfigSnapshot,
    only: Option<&ConfigTarget>,
    now_ms: i64,
) -> StoreResult<Vec<(String, i64)>> {
    let targets = match only {
        Some(t) => vec![t.clone()],
        None => s.config_targets()?,
    };
    let current: HashMap<String, (i64, String)> = match only {
        Some(t) => s
            .host_config(&t.agent_id)?
            .map(|r| (r.agent_id, (r.desired_rev, r.desired_hash)))
            .into_iter()
            .collect(),
        None => s
            .desired_all()?
            .into_iter()
            .map(|(a, rev, hash)| (a, (rev, hash)))
            .collect(),
    };
    let mut updates = Vec::new();
    for t in &targets {
        let (merged, sources, hash) = desired_of(snap, t, now_ms);
        let (old_rev, old_hash) = current
            .get(&t.agent_id)
            .cloned()
            .unwrap_or((0, String::new()));
        if hash == old_hash && (old_rev != 0 || hash.is_empty()) {
            continue;
        }
        let rev = if merged.is_empty() {
            0
        } else {
            snap.rev.max(1)
        };
        if rev == old_rev && hash == old_hash {
            continue;
        }
        updates.push(DesiredUpdate {
            agent_id: t.agent_id.clone(),
            desired_rev: rev,
            desired_hash: hash,
            desired: kairos_proto::mgmt::canonical_json(&Value::Object(merged)),
            sources: serde_json::to_string(&sources).unwrap_or_else(|_| "{}".to_string()),
        });
    }
    s.upsert_desired(&updates)?;
    Ok(updates
        .into_iter()
        .map(|u| (u.agent_id, u.desired_rev))
        .collect())
}

/// 相同優先序的衝突（REMOTE.md 第 3.4.2 節）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub assignments: (i64, i64),
    pub keys: Vec<String>,
    /// 兩者都比對到的主機數
    pub hosts: usize,
}

/// 找出相同優先序、設定了相同的鍵、且比對到同一台主機的指派。`focus` 為 `Some` 時只回報與這些指派
/// 有關的衝突（新增指派或修改設定檔時，只檢查受影響的部分）
pub fn find_conflicts(
    snap: &ConfigSnapshot,
    targets: &[ConfigTarget],
    focus: Option<&[i64]>,
    now_ms: i64,
) -> Vec<Conflict> {
    let active: Vec<&Assignment> = snap
        .assignments
        .iter()
        .filter(|a| a.active(now_ms))
        .collect();
    let keys: HashMap<i64, BTreeSet<String>> = snap
        .profiles
        .iter()
        .map(|(id, (_, s))| (*id, keys_of(s)))
        .collect();
    let mut out = Vec::new();
    for (i, a) in active.iter().enumerate() {
        for b in &active[i + 1..] {
            if a.priority != b.priority || a.profile_id == b.profile_id {
                continue;
            }
            if let Some(f) = focus
                && !f.contains(&a.id)
                && !f.contains(&b.id)
            {
                continue;
            }
            let (Some(ka), Some(kb)) = (keys.get(&a.profile_id), keys.get(&b.profile_id)) else {
                continue;
            };
            let shared: Vec<String> = ka.intersection(kb).cloned().collect();
            if shared.is_empty() {
                continue;
            }
            let hosts = targets
                .iter()
                .filter(|t| a.selector.matches(t) && b.selector.matches(t))
                .count();
            if hosts > 0 {
                out.push(Conflict {
                    assignments: (a.id, b.id),
                    keys: shared,
                    hosts,
                });
            }
        }
    }
    out
}

/// 設定變更的影響（`POST /api/v1/config/preview`、PUT 的 `impact_token`）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Impact {
    /// 期望設定會改變的主機數
    pub affected_hosts: usize,
    /// 每個鍵改變的台數
    pub by_change: BTreeMap<String, usize>,
    /// 改變的主機中，有多少台最近一次回報這些鍵被本機覆蓋
    pub overridden_locally: usize,
    /// 改變的主機中，Pulse 太舊或本機停用集中設定的台數
    pub unsupported: usize,
}

/// 比較兩份快照對所有主機的期望設定
pub fn impact(
    before: &ConfigSnapshot,
    after: &ConfigSnapshot,
    targets: &[ConfigTarget],
    overridden: &HashMap<String, BTreeSet<String>>,
    now_ms: i64,
) -> Impact {
    let mut out = Impact::default();
    for t in targets {
        let (a, _) = before.merged_for(t, now_ms);
        let (b, _) = after.merged_for(t, now_ms);
        if a == b {
            continue;
        }
        out.affected_hosts += 1;
        let fa: BTreeMap<String, Value> = flat_map(&a);
        let fb: BTreeMap<String, Value> = flat_map(&b);
        let mut changed = Vec::new();
        for k in fa.keys().chain(fb.keys()).collect::<BTreeSet<_>>() {
            if fa.get(k) != fb.get(k) {
                *out.by_change.entry(k.clone()).or_default() += 1;
                changed.push(k.clone());
            }
        }
        if overridden
            .get(&t.agent_id)
            .is_some_and(|keys| changed.iter().any(|k| keys.contains(k)))
        {
            out.overridden_locally += 1;
        }
        if !supports_config(t) {
            out.unsupported += 1;
        }
    }
    out
}

fn flat_map(m: &Map<String, Value>) -> BTreeMap<String, Value> {
    ws::flatten(m)
        .map(|f| f.into_iter().map(|(k, v)| (k, v.clone())).collect())
        .unwrap_or_default()
}

/// 主機的設定狀態分類所需的 `host_config` 欄位
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigStateInput {
    pub desired_rev: i64,
    pub applied_rev: i64,
    /// Pulse 回報的 `state`（尚未回報為 `None`）
    pub state: Option<String>,
    /// 最近一次回報有本機覆蓋的鍵
    pub overridden: bool,
}

/// 主機的設定狀態（`GET /api/v1/config/status`、主機的 `config_state`）：`unsupported`（Pulse 太舊）、
/// `disabled`、`never_reported`、`rejected`、`rolled_back`、`pending`（期望版本 ≠ 已套用）、`overridden`
/// （同步但有本機覆蓋）、`in_sync`
pub fn config_state(
    agent_version: Option<&str>,
    install_config: Option<&str>,
    row: Option<&ConfigStateInput>,
) -> &'static str {
    let new_enough = agent_version
        .and_then(|v| kairos_proto::release::Version::parse(v).ok())
        .is_some_and(|v| v.core() >= (0, 9, 0));
    if !new_enough {
        return "unsupported";
    }
    if install_config == Some(kairos_proto::mgmt::INSTALL_CONFIG_DISABLED)
        || row.and_then(|r| r.state.as_deref()) == Some(kairos_proto::mgmt::CONFIG_STATE_DISABLED)
    {
        return "disabled";
    }
    let Some(r) = row else {
        return "in_sync";
    };
    match r.state.as_deref() {
        None if r.desired_rev == 0 => "in_sync",
        None => "never_reported",
        Some("rejected") if r.applied_rev == r.desired_rev => "rejected",
        Some("rolled_back") if r.applied_rev == r.desired_rev => "rolled_back",
        _ if r.applied_rev != r.desired_rev => "pending",
        _ if r.overridden => "overridden",
        _ => "in_sync",
    }
}

/// `config_state` 的所有值（API 的篩選）
pub const CONFIG_STATES: [&str; 8] = [
    "in_sync",
    "pending",
    "rejected",
    "rolled_back",
    "overridden",
    "disabled",
    "unsupported",
    "never_reported",
];

/// `overridden` 欄位（JSON 陣列）是否有內容
pub fn has_overridden(text: Option<&str>) -> bool {
    text.and_then(|t| serde_json::from_str::<Vec<Value>>(t).ok())
        .is_some_and(|v| !v.is_empty())
}

/// 這台主機能接收集中設定：Pulse 0.9.0 以上且本機沒有停用（`install.config`）
pub fn supports_config(t: &ConfigTarget) -> bool {
    let new_enough = t
        .agent_version
        .as_deref()
        .and_then(|v| kairos_proto::release::Version::parse(v).ok())
        .is_some_and(|v| v.core() >= (0, 9, 0));
    new_enough && t.install_config.as_deref() != Some(kairos_proto::mgmt::INSTALL_CONFIG_DISABLED)
}

/// `impact_token`：影響數字與時間窗的雜湊（同 API.md 第 11.4 節）
pub fn impact_token(scope: &str, i: &Impact, now_ms: i64) -> String {
    token_at(scope, i, now_ms / crate::enroll::ops::IMPACT_WINDOW_MS)
}

fn token_at(scope: &str, i: &Impact, window: i64) -> String {
    let text = format!(
        "config|{scope}|{}|{}|{}|{window}",
        i.affected_hosts, i.overridden_locally, i.unsupported
    );
    kairos_proto::enroll::sha256_hex(text.as_bytes())[..24].to_string()
}

/// `impact_token` 是否仍符合目前的數字（目前或前一個時間窗）
pub fn verify_impact_token(scope: &str, i: &Impact, token: &str, now_ms: i64) -> bool {
    let w = now_ms / crate::enroll::ops::IMPACT_WINDOW_MS;
    token == token_at(scope, i, w) || token == token_at(scope, i, w - 1)
}

/// `GET /api/v1/config/schema`：白名單的每個鍵（主控台據此產生表單）
pub fn schema_json() -> Value {
    let keys: Vec<Value> = ws::SETTINGS
        .iter()
        .map(|s| {
            let (section, name) = s.key.rsplit_once('.').unwrap_or(("", s.key));
            let kind = match s.kind {
                Kind::Bool => json!({"type": "boolean"}),
                Kind::Int { min, max } => json!({"type": "integer", "min": min, "max": max}),
                Kind::Enum(values) => json!({"type": "enum", "values": values}),
                Kind::StringList {
                    max_items,
                    max_item_bytes,
                } => json!({"type": "string_list", "max_items": max_items, "max_item_bytes": max_item_bytes}),
            };
            let privacy = match s.privacy {
                Privacy::None => Value::Null,
                Privacy::Ordered(order) => json!({"rule": "ordered", "order": order}),
                Privacy::FalseIsConservative => json!({"rule": "false_is_conservative"}),
            };
            json!({
                "key": s.key,
                "section": section,
                "name": name,
                "value": kind,
                "apply": match s.apply { Apply::Live => "live", Apply::Reconnect => "reconnect" },
                "min_pulse": s.min_pulse,
                "privacy_restrict_only": s.privacy != Privacy::None,
                "privacy": privacy,
                "merge": match s.merge { ws::Merge::LocalWins => "local_wins", ws::Merge::Union => "union" },
                "local_only_values": s.local_only_values,
            })
        })
        .collect();
    json!({
        "keys": keys,
        "selectors": [
            {"kind": "global", "default_priority": 0},
            {"kind": "group", "default_priority": 100},
            {"kind": "tag", "default_priority": 200},
            {"kind": "host", "default_priority": 1000},
        ],
        "limits": {
            "profile_name_chars": MAX_PROFILE_NAME_CHARS,
            "description_chars": MAX_PROFILE_DESCRIPTION_CHARS,
            "settings_bytes": MAX_PROFILE_SETTINGS_BYTES,
            "impact_confirm_hosts": IMPACT_CONFIRM_HOSTS,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str, group: Option<&str>, tags: &[&str]) -> ConfigTarget {
        ConfigTarget {
            agent_id: id.to_string(),
            host_group: group.map(str::to_string),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            agent_version: Some("0.9.0".into()),
            install_config: Some("enabled".into()),
        }
    }

    fn snap(
        profiles: &[(i64, &str, Value)],
        assignments: &[(i64, i64, &str, i64)],
    ) -> ConfigSnapshot {
        let mut a: Vec<Assignment> = assignments
            .iter()
            .map(|(id, p, sel, prio)| Assignment {
                id: *id,
                profile_id: *p,
                selector: Selector::parse(sel).unwrap(),
                priority: *prio,
                expires_at: None,
            })
            .collect();
        a.sort_by_key(|x| (x.priority, x.id));
        ConfigSnapshot {
            rev: 7,
            profiles: profiles
                .iter()
                .map(|(id, n, v)| (*id, (n.to_string(), v.as_object().unwrap().clone())))
                .collect(),
            assignments: a,
        }
    }

    const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
    const B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    #[test]
    fn selectors_parse_and_match() {
        assert_eq!(Selector::parse("global").unwrap(), Selector::Global);
        assert_eq!(
            Selector::parse("group:Prod-Web").unwrap(),
            Selector::Group("prod-web".into())
        );
        assert_eq!(
            Selector::parse("tag:env:prod").unwrap(),
            Selector::Tag("env:prod".into())
        );
        assert_eq!(
            Selector::parse(&format!("host:{}", A.to_uppercase())).unwrap(),
            Selector::Host(A.into())
        );
        for bad in ["", "all", "group:", "host:web-01", "tag:", "kind:x"] {
            assert!(Selector::parse(bad).is_err(), "{bad}");
        }
        let t = target(A, Some("Prod-Web"), &["env:prod"]);
        assert!(Selector::Group("prod-web".into()).matches(&t));
        assert!(Selector::Tag("env:prod".into()).matches(&t));
        assert!(
            !Selector::Tag("ENV:prod".into()).matches(&t),
            "tags are case sensitive"
        );
        assert!(Selector::Host(A.into()).matches(&t));
        assert!(!Selector::Host(B.into()).matches(&t));
        assert_eq!(Selector::Tag("x".into()).default_priority(), 200);
    }

    #[test]
    fn layers_are_applied_by_priority_and_sources_follow() {
        let s = snap(
            &[
                (
                    1,
                    "全站",
                    json!({"report": {"interval": 10}, "log": {"level": "info"}}),
                ),
                (2, "web 除錯", json!({"log": {"level": "debug"}})),
            ],
            &[(1, 1, "global", 0), (2, 2, &format!("host:{A}"), 1000)],
        );
        let (m, src) = s.merged_for(&target(A, None, &[]), 0);
        assert_eq!(
            m,
            *json!({"report": {"interval": 10}, "log": {"level": "debug"}})
                .as_object()
                .unwrap()
        );
        assert_eq!(src["log.level"], "web 除錯");
        assert_eq!(src["report.interval"], "全站");
        let (m, _) = s.merged_for(&target(B, None, &[]), 0);
        assert_eq!(m["log"]["level"], "info");
    }

    #[test]
    fn expired_assignments_are_ignored() {
        let mut s = snap(
            &[(1, "p", json!({"log": {"level": "debug"}}))],
            &[(1, 1, "global", 0)],
        );
        s.assignments[0].expires_at = Some(1000);
        assert!(!s.merged_for(&target(A, None, &[]), 999).0.is_empty());
        assert!(s.merged_for(&target(A, None, &[]), 1000).0.is_empty());
    }

    #[test]
    fn conflicts_need_same_priority_shared_keys_and_shared_hosts() {
        let s = snap(
            &[
                (1, "a", json!({"log": {"level": "debug"}})),
                (2, "b", json!({"log": {"level": "warn"}})),
                (3, "c", json!({"report": {"interval": 5}})),
            ],
            &[
                (1, 1, "tag:env:prod", 200),
                (2, 2, "tag:dc:tp", 200),
                (3, 3, "tag:dc:tp", 200),
            ],
        );
        let targets = vec![
            target(A, None, &["env:prod", "dc:tp"]),
            target(B, None, &["dc:tp"]),
        ];
        let c = find_conflicts(&s, &targets, None, 0);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].assignments, (1, 2));
        assert_eq!(c[0].keys, vec!["log.level".to_string()]);
        assert_eq!(c[0].hosts, 1);
        // 沒有共同的主機：不算衝突
        let targets = vec![target(A, None, &["env:prod"]), target(B, None, &["dc:tp"])];
        assert!(find_conflicts(&s, &targets, None, 0).is_empty());
        // 只檢查 focus 有關的
        let targets = vec![target(A, None, &["env:prod", "dc:tp"])];
        assert!(find_conflicts(&s, &targets, Some(&[3]), 0).is_empty());
    }

    #[test]
    fn profile_settings_are_checked_and_canonicalized() {
        let (m, text) =
            check_profile_settings(&json!({"report": {"interval": 10}, "log": {}})).unwrap();
        assert_eq!(text, r#"{"report":{"interval":10}}"#);
        assert_eq!(m.len(), 1);
        let e = check_profile_settings(&json!({"server": {"address": "x"}})).unwrap_err();
        assert_eq!(e[0].reason, "unknown_key");
        let e = check_profile_settings(&json!({"processes": {"command": "full"}})).unwrap_err();
        assert_eq!(e[0].key, "processes.command");
        let e = check_profile_settings(&json!({"report": {"interval": 0}})).unwrap_err();
        assert_eq!(e[0].reason, "out_of_range");
        assert!(check_profile_settings(&json!([1])).is_err());
        assert!(check_profile_name(" 正式環境 Linux ").is_ok());
        assert!(check_profile_name("").is_err());
        assert!(check_profile_name(&"x".repeat(129)).is_err());
    }

    #[test]
    fn impact_counts_changed_hosts_and_keys() {
        let before = snap(
            &[(1, "p", json!({"log": {"level": "info"}}))],
            &[(1, 1, "global", 0)],
        );
        let after = snap(
            &[(
                1,
                "p",
                json!({"log": {"level": "debug"}, "report": {"interval": 5}}),
            )],
            &[(1, 1, "global", 0)],
        );
        let mut old = target(B, None, &[]);
        old.agent_version = Some("0.8.0".into());
        let targets = vec![target(A, None, &[]), old];
        let overridden: HashMap<String, BTreeSet<String>> =
            [(A.to_string(), ["report.interval".to_string()].into())].into();
        let i = impact(&before, &after, &targets, &overridden, 0);
        assert_eq!(i.affected_hosts, 2);
        assert_eq!(i.by_change["log.level"], 2);
        assert_eq!(i.by_change["report.interval"], 2);
        assert_eq!(i.overridden_locally, 1);
        assert_eq!(i.unsupported, 1);
        let t = impact_token("profile:1", &i, 1_000_000);
        assert!(verify_impact_token("profile:1", &i, &t, 1_000_000));
        assert!(!verify_impact_token("profile:2", &i, &t, 1_000_000));
    }

    #[test]
    fn config_states_are_classified() {
        let row = |d: i64, a: i64, st: Option<&str>, o: bool| ConfigStateInput {
            desired_rev: d,
            applied_rev: a,
            state: st.map(str::to_string),
            overridden: o,
        };
        let v = Some("0.9.0");
        assert_eq!(config_state(Some("0.8.0"), None, None), "unsupported");
        assert_eq!(config_state(None, None, None), "unsupported");
        assert_eq!(config_state(v, Some("disabled"), None), "disabled");
        assert_eq!(
            config_state(v, None, Some(&row(3, 3, Some("disabled"), false))),
            "disabled"
        );
        assert_eq!(config_state(v, None, None), "in_sync");
        assert_eq!(
            config_state(v, None, Some(&row(0, 0, None, false))),
            "in_sync"
        );
        assert_eq!(
            config_state(v, None, Some(&row(3, 0, None, false))),
            "never_reported"
        );
        assert_eq!(
            config_state(v, None, Some(&row(3, 3, Some("rejected"), false))),
            "rejected"
        );
        assert_eq!(
            config_state(v, None, Some(&row(3, 3, Some("rolled_back"), false))),
            "rolled_back"
        );
        assert_eq!(
            config_state(v, None, Some(&row(4, 3, Some("applied"), false))),
            "pending"
        );
        assert_eq!(
            config_state(v, None, Some(&row(4, 3, Some("rejected"), false))),
            "pending"
        );
        assert_eq!(
            config_state(v, None, Some(&row(3, 3, Some("applied"), true))),
            "overridden"
        );
        assert_eq!(
            config_state(v, None, Some(&row(3, 3, Some("applied"), false))),
            "in_sync"
        );
        assert!(has_overridden(Some(r#"[{"key":"x"}]"#)));
        assert!(!has_overridden(Some("[]")));
        assert!(!has_overridden(None));
    }

    #[test]
    fn schema_lists_every_whitelisted_key() {
        let s = schema_json();
        assert_eq!(s["keys"].as_array().unwrap().len(), ws::SETTINGS.len());
        let cmd = s["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["key"] == "processes.command")
            .unwrap();
        assert_eq!(cmd["privacy_restrict_only"], true);
        assert_eq!(cmd["local_only_values"], json!(["full"]));
    }
}
