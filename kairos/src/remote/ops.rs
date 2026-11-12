//! 集中設定與版本庫的寫入操作（主控台 API、CLI 共用）：每個操作一個 Immediate 交易，同一個交易中寫稽核
//! （REMOTE.md 第 5.3 節）並重新計算受影響主機的期望設定；成功後更新記憶體中的快照與 `desired_rev`。
//! 推送的操作與推送引擎在 [`super::engine`]。

use hyper::StatusCode;
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};

use super::desired::{
    self, Assignment, ConfigSnapshot, IMPACT_CONFIRM_HOSTS, Impact, MAX_ASSIGNMENTS, MAX_PROFILES,
    Selector,
};
use super::releases::{self, ImportError, ImportRequest, Upload};
use super::store::{ArtifactRow, ConfigTarget, ProfileRow, ReleaseRow};
use super::{CODE_CONFIG_INVALID, CODE_IMPORT_FAILED, RemoteService};
use crate::api::http::ApiError;
use crate::audit::Event;
use crate::enroll::db::DbError;
use crate::store::db::{Store, StoreError};

/// 執行者（稽核的 `actor`、`actor_ip`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// `user:<帳號>`、`token:<token_id>`、`cli:<OS 帳號>`、`system`
    pub actor: String,
    pub ip: Option<String>,
}

impl Actor {
    pub fn system() -> Actor {
        Actor {
            actor: "system".to_string(),
            ip: None,
        }
    }

    pub(crate) fn event(
        &self,
        category: &'static str,
        action: &'static str,
        object: Option<String>,
        code: Option<&'static str>,
        details: Value,
    ) -> Event {
        Event {
            actor: self.actor.clone(),
            actor_ip: self.ip.clone(),
            category,
            action,
            object,
            result: if code.is_some_and(|c| c.contains("-E")) {
                "failed"
            } else {
                "ok"
            },
            code,
            details: Some(details),
        }
    }
}

/// 交易中的錯誤：資料庫（503）或要回給呼叫端的錯誤（交易回復）
#[derive(Debug)]
pub enum TxError {
    Db(DbError),
    Api(ApiError),
}

impl From<StoreError> for TxError {
    fn from(e: StoreError) -> Self {
        TxError::Db(DbError::from(e))
    }
}

impl From<DbError> for TxError {
    fn from(e: DbError) -> Self {
        TxError::Db(e)
    }
}

impl From<ApiError> for TxError {
    fn from(e: ApiError) -> Self {
        TxError::Api(e)
    }
}

impl TxError {
    /// 轉成 API 的錯誤（資料庫錯誤記錄後回 503 `storage_unavailable`）
    pub fn into_api(self, what: &str) -> ApiError {
        match self {
            TxError::Api(e) => e,
            TxError::Db(e) => {
                log::warn!("遠端管理：{what}失敗：{e}");
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_unavailable",
                    format!("無法保存{what}：資料庫暫時無法使用"),
                )
                .with_product_code("KRS-E4003")
            }
        }
    }
}

/// 400 `config_invalid`（`KRS-E0502`）
pub fn config_invalid(errors: &[kairos_proto::mgmt::KeyError]) -> ApiError {
    let first = errors.first();
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "config_invalid",
        match first {
            Some(e) => format!("設定檔無法儲存：{} {}", e.key, e.message),
            None => "設定檔無法儲存".to_string(),
        },
    )
    .with_product_code(CODE_CONFIG_INVALID)
    .with_details(json!({
        "key": first.map(|e| e.key.clone()),
        "reason": first.map(|e| e.reason.clone()),
        "errors": errors.iter().map(|e| json!({"key": e.key, "reason": e.reason, "message": e.message})).collect::<Vec<_>>(),
    }))
}

fn param(name: &str, msg: impl Into<String>) -> ApiError {
    ApiError::param(name, msg)
}

fn not_found(code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, code, msg)
}

/// 本文中的原因（選填，最多 500 個字元）
pub fn reason_of(body: &Value) -> Result<Option<String>, ApiError> {
    match body.get("reason") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(r)) => {
            let r = r.trim();
            if r.chars().count() > desired::MAX_REASON_CHARS
                || r.chars()
                    .any(|c| c != '\n' && kairos_proto::is_forbidden_display_char(c))
            {
                return Err(param("reason", "原因最多 500 個字元、不含控制字元"));
            }
            Ok((!r.is_empty()).then(|| r.to_string()))
        }
        Some(_) => Err(param("reason", "reason 必須是字串")),
    }
}

/// 設定檔的 JSON（清單與詳情）
pub fn profile_json(p: &ProfileRow, assignments: usize, hosts: usize) -> Value {
    let settings = desired::parse_settings(&p.settings);
    json!({
        "id": p.id,
        "name": p.name,
        "description": p.description,
        "settings": settings,
        "keys": desired::keys_of(&settings).len(),
        "revision": p.revision,
        "assignments": assignments,
        "hosts": hosts,
        "created_at": crate::api::http::ts_ms(p.created_at),
        "created_by": p.created_by,
        "updated_at": crate::api::http::ts_ms(p.updated_at),
        "updated_by": p.updated_by,
    })
}

/// 指派的 JSON
pub fn assignment_json(
    a: &super::store::AssignmentRow,
    profile_name: Option<&str>,
    hosts: usize,
) -> Value {
    let sel = Selector::from_db(&a.selector_kind, &a.selector_value);
    json!({
        "id": a.id,
        "profile_id": a.profile_id,
        "profile_name": profile_name,
        "selector": sel.as_ref().map(Selector::text),
        "selector_kind": a.selector_kind,
        "selector_value": a.selector_value,
        "priority": a.priority,
        "expires_at": a.expires_at.map(crate::api::http::ts_ms),
        "hosts": hosts,
        "created_at": crate::api::http::ts_ms(a.created_at),
        "created_by": a.created_by,
    })
}

/// 衝突的 409（`details.keys`、`hosts`、`assignments`）
fn conflict_error(c: &desired::Conflict) -> ApiError {
    ApiError::conflict(
        "assignment_conflict",
        format!(
            "與指派 #{} 的優先序相同，且同時設定了 {}（{} 台主機兩者都比對到）；請改變優先序或移除重複的鍵",
            if c.assignments.0 == 0 { c.assignments.1 } else { c.assignments.0 },
            c.keys.join("、"),
            c.hosts
        ),
    )
    .with_details(json!({
        "keys": c.keys,
        "hosts": c.hosts,
        "assignments": [c.assignments.0, c.assignments.1],
    }))
}

/// 各主機最近一次回報被本機覆蓋的鍵（影響預覽）
fn overridden_keys(s: &dyn Store) -> Result<HashMap<String, BTreeSet<String>>, StoreError> {
    Ok(s.host_configs()?
        .into_iter()
        .filter_map(|r| {
            let list: Vec<Value> = serde_json::from_str(r.overridden.as_deref()?).ok()?;
            let keys: BTreeSet<String> = list
                .iter()
                .filter_map(|o| o.get("key").and_then(Value::as_str).map(str::to_string))
                .collect();
            (!keys.is_empty()).then_some((r.agent_id, keys))
        })
        .collect())
}

/// 一次設定變更的計畫：變更前後的快照、主機、影響
pub(crate) struct ChangePlan {
    pub before: ConfigSnapshot,
    pub after: ConfigSnapshot,
    pub targets: Vec<ConfigTarget>,
    pub impact: Impact,
}

pub(crate) fn plan_change(
    s: &dyn Store,
    now_ms: i64,
    mutate: impl FnOnce(&mut ConfigSnapshot),
) -> Result<ChangePlan, StoreError> {
    let before = ConfigSnapshot::load(s)?;
    let mut after = before.clone();
    mutate(&mut after);
    after.assignments.sort_by_key(|a| (a.priority, a.id));
    let targets = s.config_targets()?;
    let overridden = overridden_keys(s)?;
    let impact = desired::impact(&before, &after, &targets, &overridden, now_ms);
    Ok(ChangePlan {
        before,
        after,
        targets,
        impact,
    })
}

/// 影響範圍的 JSON（預覽與 409 `impact_changed`）
pub fn impact_json(scope: &str, i: &Impact, now_ms: i64) -> Value {
    json!({
        "affected_hosts": i.affected_hosts,
        "by_change": i.by_change,
        "overridden_locally": i.overridden_locally,
        "unsupported": i.unsupported,
        "confirm_required": i.affected_hosts >= IMPACT_CONFIRM_HOSTS,
        "impact_token": desired::impact_token(scope, i, now_ms),
    })
}

/// 影響 1,000 台以上時需要 `impact_token`（REMOTE.md 第 8.1.1 節）
fn check_impact(scope: &str, i: &Impact, token: Option<&str>, now_ms: i64) -> Result<(), ApiError> {
    if i.affected_hosts < IMPACT_CONFIRM_HOSTS {
        return Ok(());
    }
    match token {
        None => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            format!(
                "這個變更會套用到 {} 台主機，需要先預覽並帶 impact_token",
                i.affected_hosts
            ),
        )
        .with_details(impact_json(scope, i, now_ms))),
        Some(t) if desired::verify_impact_token(scope, i, t, now_ms) => Ok(()),
        Some(_) => Err(
            ApiError::conflict("impact_changed", "影響數字在確認後改變，請重新確認")
                .with_details(impact_json(scope, i, now_ms)),
        ),
    }
}

/// 設定變更的提交：加版本、重新計算、回傳改變的主機
fn commit_change(
    s: &dyn Store,
    now_ms: i64,
) -> Result<(ConfigSnapshot, Vec<(String, i64)>), StoreError> {
    s.bump_remote_config_rev()?;
    let snap = ConfigSnapshot::load(s)?;
    let changed = desired::recompute(s, &snap, None, now_ms)?;
    Ok((snap, changed))
}

/// 兩份設定的差異（稽核的變更前後）
fn diff(before: &Map<String, Value>, after: &Map<String, Value>) -> Value {
    let flat = |m: &Map<String, Value>| -> std::collections::BTreeMap<String, Value> {
        kairos_proto::mgmt::settings::flatten(m)
            .map(|f| f.into_iter().map(|(k, v)| (k, v.clone())).collect())
            .unwrap_or_default()
    };
    let (a, b) = (flat(before), flat(after));
    let mut out = Map::new();
    for k in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
        if a.get(k) != b.get(k) {
            out.insert(k.clone(), json!({"before": a.get(k), "after": b.get(k)}));
        }
    }
    Value::Object(out)
}

/// 新建或修改設定檔的本文
#[derive(Debug, Clone)]
pub struct ProfileInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub settings: Option<Value>,
    pub reason: Option<String>,
    pub impact_token: Option<String>,
}

impl RemoteService {
    /// 重新計算後更新記憶體中的狀態
    fn after_change(&self, snap: ConfigSnapshot, changed: &[(String, i64)]) {
        self.set_snapshot(snap);
        self.apply_desired(changed);
        if !changed.is_empty() {
            log::info!(
                event = "config.recomputed";
                "集中設定：第 {} 版，{} 台主機的期望設定改變（下一次心跳時通知）",
                self.snapshot().rev,
                changed.len()
            );
        }
    }

    /// `POST /api/v1/config/profiles`
    pub async fn create_profile(
        &self,
        who: &Actor,
        input: ProfileInput,
    ) -> Result<Value, ApiError> {
        let name = desired::check_profile_name(input.name.as_deref().unwrap_or(""))
            .map_err(|e| param("name", e))?;
        let description = desired::check_description(input.description.as_deref().unwrap_or(""))
            .map_err(|e| param("description", e))?;
        let (settings, text) =
            desired::check_profile_settings(input.settings.as_ref().unwrap_or(&json!({})))
                .map_err(|e| config_invalid(&e))?;
        let now = self.now_ms();
        let who = who.clone();
        let reason = input.reason.clone();
        let (row, snap, changed) = self
            .in_tx(move |s| {
                if s.profile_id_by_name(&name)?.is_some() {
                    return Err(ApiError::conflict("profile_exists", format!("已有名為「{name}」的設定檔")).into());
                }
                if s.list_profiles()?.len() >= MAX_PROFILES {
                    return Err(ApiError::conflict("too_many_profiles", format!("設定檔最多 {MAX_PROFILES} 個")).into());
                }
                let id = s.insert_profile(&name, &description, &text, now, &who.actor, reason.as_deref())?;
                let (snap, changed) = commit_change(s, now)?;
                who.event(
                    "config",
                    "config.profile.create",
                    Some(format!("profile:{id}")),
                    None,
                    json!({"profile_id": id, "name": name, "settings": settings, "reason": reason, "config_rev": snap.rev}),
                )
                .append(s, now)?;
                let row = s.get_profile(id)?.ok_or_else(|| DbError("設定檔寫入後讀不到".into()))?;
                Ok((row, snap, changed))
            })
            .await
            .map_err(|e| e.into_api("設定檔"))?;
        self.after_change(snap, &changed);
        Ok(profile_json(&row, 0, 0))
    }

    /// `PUT /api/v1/config/profiles/{id}`（`revert` 時 `revert_of` 為舊的版本號）
    pub async fn update_profile(
        &self,
        who: &Actor,
        id: i64,
        input: ProfileInput,
        revert_of: Option<i64>,
    ) -> Result<Value, ApiError> {
        let name = match &input.name {
            Some(n) => Some(desired::check_profile_name(n).map_err(|e| param("name", e))?),
            None => None,
        };
        let description = match &input.description {
            Some(d) => Some(desired::check_description(d).map_err(|e| param("description", e))?),
            None => None,
        };
        let settings = match &input.settings {
            Some(v) => Some(desired::check_profile_settings(v).map_err(|e| config_invalid(&e))?),
            None => None,
        };
        let now = self.now_ms();
        let who = who.clone();
        let (row, snap, changed, assigned) = self
            .in_tx(move |s| {
                let cur = s
                    .get_profile(id)?
                    .ok_or_else(|| not_found("profile_not_found", format!("沒有設定檔 #{id}")))?;
                let (new_settings, new_text) = match (revert_of, settings) {
                    (Some(rev), _) => {
                        let r = s
                            .profile_revisions(id)?
                            .into_iter()
                            .find(|r| r.revision == rev)
                            .ok_or_else(|| param("revision", format!("設定檔沒有第 {rev} 版")))?;
                        let v = serde_json::from_str::<Value>(&r.settings).unwrap_or(json!({}));
                        desired::check_profile_settings(&v)
                            .map_err(|e| TxError::Api(config_invalid(&e)))?
                    }
                    (None, Some(x)) => x,
                    (None, None) => {
                        let m = desired::parse_settings(&cur.settings);
                        let t = kairos_proto::mgmt::canonical_json(&Value::Object(m.clone()));
                        (m, t)
                    }
                };
                let new_name = name.unwrap_or_else(|| cur.name.clone());
                if new_name.to_lowercase() != cur.name.to_lowercase()
                    && s.profile_id_by_name(&new_name)?.is_some()
                {
                    return Err(ApiError::conflict(
                        "profile_exists",
                        format!("已有名為「{new_name}」的設定檔"),
                    )
                    .into());
                }
                let new_desc = description.unwrap_or_else(|| cur.description.clone());
                let ns = new_settings.clone();
                let plan = plan_change(s, now, |snap| {
                    if let Some(p) = snap.profiles.get_mut(&id) {
                        p.0 = new_name.clone();
                        p.1 = ns;
                    }
                })?;
                let focus: Vec<i64> = plan
                    .after
                    .assignments
                    .iter()
                    .filter(|a| a.profile_id == id)
                    .map(|a| a.id)
                    .collect();
                if let Some(c) =
                    desired::find_conflicts(&plan.after, &plan.targets, Some(&focus), now).first()
                {
                    return Err(conflict_error(c).into());
                }
                let scope = format!("profile:{id}");
                check_impact(&scope, &plan.impact, input.impact_token.as_deref(), now)?;
                let reason = input.reason.clone();
                let revision = s.update_profile(
                    id,
                    &new_name,
                    &new_desc,
                    &new_text,
                    now,
                    &who.actor,
                    reason.as_deref(),
                )?;
                let (snap, changed) = commit_change(s, now)?;
                let old = desired::parse_settings(&cur.settings);
                who.event(
                    "config",
                    if revert_of.is_some() {
                        "config.profile.revert"
                    } else {
                        "config.profile.update"
                    },
                    Some(format!("profile:{id}")),
                    None,
                    json!({
                        "profile_id": id,
                        "name": new_name,
                        "revision": revision,
                        "reverted_to": revert_of,
                        "changes": diff(&old, &new_settings),
                        "renamed_from": (cur.name != new_name).then(|| cur.name.clone()),
                        "affected_hosts": plan.impact.affected_hosts,
                        "reason": reason,
                        "config_rev": snap.rev,
                    }),
                )
                .append(s, now)?;
                let row = s
                    .get_profile(id)?
                    .ok_or_else(|| DbError("設定檔寫入後讀不到".into()))?;
                Ok((row, snap, changed, focus.len()))
            })
            .await
            .map_err(|e| e.into_api("設定檔"))?;
        self.after_change(snap, &changed);
        Ok(profile_json(&row, assigned, changed.len()))
    }

    /// `DELETE /api/v1/config/profiles/{id}`：有指派時 409 `profile_in_use`
    pub async fn delete_profile(
        &self,
        who: &Actor,
        id: i64,
        reason: Option<String>,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let snap = self
            .in_tx(move |s| {
                let cur = s
                    .get_profile(id)?
                    .ok_or_else(|| not_found("profile_not_found", format!("沒有設定檔 #{id}")))?;
                let used: Vec<i64> = s
                    .list_assignments()?
                    .into_iter()
                    .filter(|a| a.profile_id == id)
                    .map(|a| a.id)
                    .collect();
                if !used.is_empty() {
                    return Err(ApiError::conflict(
                        "profile_in_use",
                        format!("設定檔「{}」仍有 {} 個指派，請先解除指派", cur.name, used.len()),
                    )
                    .with_details(json!({"assignments": used}))
                    .into());
                }
                s.delete_profile(id)?;
                let (snap, _) = commit_change(s, now)?;
                who.event(
                    "config",
                    "config.profile.delete",
                    Some(format!("profile:{id}")),
                    None,
                    json!({"profile_id": id, "name": cur.name, "revision": cur.revision, "settings": desired::parse_settings(&cur.settings), "reason": reason}),
                )
                .append(s, now)?;
                Ok(snap)
            })
            .await
            .map_err(|e| e.into_api("設定檔"))?;
        self.after_change(snap, &[]);
        Ok(json!({"id": id, "deleted": true}))
    }

    /// `POST /api/v1/config/assignments`
    pub async fn create_assignment(
        &self,
        who: &Actor,
        profile_id: i64,
        selector: Selector,
        priority: Option<i64>,
        expires_at: Option<i64>,
        impact_token: Option<String>,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        if expires_at.is_some_and(|t| t <= now) {
            return Err(param("expires_at", "expires_at 必須是未來的時間"));
        }
        let priority = priority.unwrap_or_else(|| selector.default_priority());
        if !(-1_000_000..=1_000_000).contains(&priority) {
            return Err(param("priority", "priority 必須介於 -1000000..=1000000"));
        }
        let who = who.clone();
        let (row, name, snap, changed) = self
            .in_tx(move |s| {
                let profile = s.get_profile(profile_id)?.ok_or_else(|| {
                    not_found("profile_not_found", format!("沒有設定檔 #{profile_id}"))
                })?;
                let existing = s.list_assignments()?;
                if existing.len() >= MAX_ASSIGNMENTS {
                    return Err(ApiError::conflict(
                        "too_many_assignments",
                        format!("指派最多 {MAX_ASSIGNMENTS} 個"),
                    )
                    .into());
                }
                let (kind, value) = selector.to_db();
                if selector == Selector::Global
                    && existing.iter().any(|a| a.selector_kind == "global")
                {
                    return Err(ApiError::conflict(
                        "global_exists",
                        "已有一個全站（global）的指派；global 最多一筆",
                    )
                    .into());
                }
                if existing.iter().any(|a| {
                    a.selector_kind == kind
                        && a.selector_value == value
                        && a.profile_id == profile_id
                }) {
                    return Err(ApiError::conflict(
                        "assignment_exists",
                        "這個設定檔已指派給同一個選擇器",
                    )
                    .into());
                }
                let candidate = Assignment {
                    id: 0,
                    profile_id,
                    selector: selector.clone(),
                    priority,
                    expires_at,
                };
                let plan = plan_change(s, now, |snap| snap.assignments.push(candidate))?;
                if let Some(c) =
                    desired::find_conflicts(&plan.after, &plan.targets, Some(&[0]), now).first()
                {
                    return Err(conflict_error(c).into());
                }
                let scope = format!("assign:{}:{profile_id}:{priority}", selector.text());
                check_impact(&scope, &plan.impact, impact_token.as_deref(), now)?;
                let id = s
                    .insert_assignment(
                        profile_id, kind, value, priority, expires_at, now, &who.actor,
                    )?
                    .ok_or_else(|| {
                        TxError::Api(ApiError::conflict(
                            "assignment_exists",
                            "這個設定檔已指派給同一個選擇器",
                        ))
                    })?;
                let (snap, changed) = commit_change(s, now)?;
                who.event(
                    "config",
                    "config.assign",
                    Some(format!("assignment:{id}")),
                    None,
                    json!({
                        "assignment_id": id,
                        "profile_id": profile_id,
                        "profile_name": profile.name,
                        "selector": selector.text(),
                        "priority": priority,
                        "expires_at": expires_at.map(crate::api::http::ts_ms),
                        "affected_hosts": plan.impact.affected_hosts,
                        "config_rev": snap.rev,
                    }),
                )
                .append(s, now)?;
                let row = s
                    .get_assignment(id)?
                    .ok_or_else(|| DbError("指派寫入後讀不到".into()))?;
                Ok((row, profile.name, snap, changed))
            })
            .await
            .map_err(|e| e.into_api("指派"))?;
        let hosts = changed.len();
        self.after_change(snap, &changed);
        Ok(assignment_json(&row, Some(&name), hosts))
    }

    /// `DELETE /api/v1/config/assignments/{id}`
    pub async fn delete_assignment(
        &self,
        who: &Actor,
        id: i64,
        impact_token: Option<String>,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let (snap, changed) = self
            .in_tx(move |s| {
                let a = s
                    .get_assignment(id)?
                    .ok_or_else(|| not_found("assignment_not_found", format!("沒有指派 #{id}")))?;
                let plan = plan_change(s, now, |snap| snap.assignments.retain(|x| x.id != id))?;
                check_impact(&format!("unassign:{id}"), &plan.impact, impact_token.as_deref(), now)?;
                s.delete_assignment(id)?;
                let (snap, changed) = commit_change(s, now)?;
                let profile = s.get_profile(a.profile_id)?.map(|p| p.name);
                who.event(
                    "config",
                    "config.unassign",
                    Some(format!("assignment:{id}")),
                    None,
                    json!({
                        "assignment_id": id,
                        "profile_id": a.profile_id,
                        "profile_name": profile,
                        "selector": Selector::from_db(&a.selector_kind, &a.selector_value).map(|s| s.text()),
                        "priority": a.priority,
                        "affected_hosts": plan.impact.affected_hosts,
                        "config_rev": snap.rev,
                    }),
                )
                .append(s, now)?;
                Ok((snap, changed))
            })
            .await
            .map_err(|e| e.into_api("指派"))?;
        let hosts = changed.len();
        self.after_change(snap, &changed);
        Ok(json!({"id": id, "deleted": true, "affected_hosts": hosts}))
    }

    /// 到期的指派：刪除、記 `config.assign_expired`（`system`）、重新計算
    pub(crate) async fn expire_assignments(&self) -> Result<(), DbError> {
        let now = self.now_ms();
        let r = self
            .in_tx(move |s| {
                let expired = s.expired_assignments(now)?;
                if expired.is_empty() {
                    return Ok(None);
                }
                for a in &expired {
                    s.delete_assignment(a.id)?;
                }
                let (snap, changed) = commit_change(s, now)?;
                for a in &expired {
                    Actor::system()
                        .event(
                            "config",
                            "config.assign_expired",
                            Some(format!("assignment:{}", a.id)),
                            None,
                            json!({
                                "assignment_id": a.id,
                                "profile_id": a.profile_id,
                                "selector": Selector::from_db(&a.selector_kind, &a.selector_value).map(|s| s.text()),
                                "expires_at": a.expires_at.map(crate::api::http::ts_ms),
                                "config_rev": snap.rev,
                            }),
                        )
                        .append(s, now)?;
                }
                Ok(Some((snap, changed, expired.len())))
            })
            .await;
        match r {
            Ok(Some((snap, changed, n))) => {
                log::info!(event = "config.assign_expired"; "集中設定：{n} 個指派已到期並刪除");
                self.after_change(snap, &changed);
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(TxError::Db(e)) => Err(e),
            Err(TxError::Api(e)) => Err(DbError(e.message)),
        }
    }

    /// `POST /api/v1/config/preview`：假設的變更的影響（不寫入）
    pub async fn preview_change(&self, change: PreviewChange) -> Result<Value, ApiError> {
        let now = self.now_ms();
        self.db
            .read(move |s| {
                let mut scope = String::new();
                let mut bad: Option<ApiError> = None;
                let plan = plan_change(s, now, |snap| match &change {
                    PreviewChange::Profile { id, settings } => {
                        scope = format!("profile:{id}");
                        match snap.profiles.get_mut(id) {
                            Some(p) => p.1 = settings.clone(),
                            None => bad = Some(not_found("profile_not_found", format!("沒有設定檔 #{id}"))),
                        }
                    }
                    PreviewChange::Assign { profile_id, selector, priority, expires_at } => {
                        scope = format!("assign:{}:{profile_id}:{priority}", selector.text());
                        if !snap.profiles.contains_key(profile_id) {
                            bad = Some(not_found("profile_not_found", format!("沒有設定檔 #{profile_id}")));
                        }
                        snap.assignments.push(Assignment {
                            id: 0,
                            profile_id: *profile_id,
                            selector: selector.clone(),
                            priority: *priority,
                            expires_at: *expires_at,
                        });
                    }
                    PreviewChange::Unassign { id } => {
                        scope = format!("unassign:{id}");
                        if !snap.assignments.iter().any(|a| a.id == *id) {
                            bad = Some(not_found("assignment_not_found", format!("沒有指派 #{id}")));
                        }
                        snap.assignments.retain(|a| a.id != *id);
                    }
                })?;
                if let Some(e) = bad {
                    return Ok(Err(e));
                }
                let conflicts: Vec<Value> = desired::find_conflicts(&plan.after, &plan.targets, None, now)
                    .into_iter()
                    .filter(|c| !desired::find_conflicts(&plan.before, &plan.targets, None, now).contains(c))
                    .map(|c| json!({"assignments": [c.assignments.0, c.assignments.1], "keys": c.keys, "hosts": c.hosts}))
                    .collect();
                let mut v = impact_json(&scope, &plan.impact, now);
                v["conflicts"] = json!(conflicts);
                Ok(Ok(v))
            })
            .await
            .map_err(|e| {
                log::warn!("集中設定：影響預覽失敗：{e}");
                ApiError::storage("影響預覽")
            })?
    }

    // ---- 版本庫 ----

    /// 載入 `[remote] allowed_signers`（沒有設定時 `None`）
    pub fn trust(&self) -> Result<Option<releases::Trust>, String> {
        match self.settings.allowed_signers_path(&self.data_dir) {
            None => Ok(None),
            Some(p) => releases::load_trust(&p, self.settings.principals()).map(Some),
        }
    }

    /// 匯入 Kairos 代管的版本（測試用的簡寫；API 與 CLI 用 [`Self::import_release_from`]）
    #[cfg(test)]
    pub async fn import_release(
        &self,
        who: &Actor,
        sha256sums: String,
        signature: String,
        manifest: Option<String>,
    ) -> Result<Value, ApiError> {
        self.import_release_from(who, sha256sums, signature, manifest, None)
            .await
    }

    /// `POST /api/v1/releases`、`kairos releases import`：驗證並建立版本（發佈檔另外上傳）。
    /// `base_url` 有值時為外部 URL 的版本庫（`source: external`，REMOTE.md 第 4.1.2 節）：只登記 `SHA256SUMS`、簽章與清單（驗證與代管的完全相同），Kairos 不保存發佈檔，
    /// 提議的 `artifact.url` 為 `base_url` ＋檔名，由 Pulse 依本機白名單下載並以同一份簽章驗證
    pub async fn import_release_from(
        &self,
        who: &Actor,
        sha256sums: String,
        signature: String,
        manifest: Option<String>,
        base_url: Option<String>,
    ) -> Result<Value, ApiError> {
        let base_url = match base_url {
            None => None,
            Some(u) => Some(
                kairos_proto::mgmt::url::normalize_base_url(&u)
                    .map_err(|e| param("base_url", format!("base_url 不合法：{e}")))?,
            ),
        };
        let now = self.now_ms();
        let trust = self.trust().map_err(|e| {
            log::error!("遠端管理：remote.allowed_signers 無法使用：{e}");
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "trust_anchor_unavailable",
                format!("remote.allowed_signers 無法使用：{e}"),
            )
            .with_product_code(CODE_IMPORT_FAILED)
        })?;
        let req = ImportRequest {
            sha256sums: &sha256sums,
            signature: &signature,
            manifest: manifest.as_deref(),
        };
        let verified = match releases::verify_release(&req, trust.as_ref(), now / 1000) {
            Ok(v) => v,
            Err(e) => {
                let (code, msg) = match &e {
                    ImportError::Signature(m) => ("release_signature_invalid", m.clone()),
                    ImportError::Invalid(m) => ("release_invalid", m.clone()),
                };
                log::warn!(event = "release.import_failed", code = CODE_IMPORT_FAILED; "錯誤 {CODE_IMPORT_FAILED}：無法匯入 Pulse 發佈版本：{msg}");
                let ev = who.event(
                    "release",
                    "release.import_failed",
                    None,
                    Some(CODE_IMPORT_FAILED),
                    json!({"reason": code, "message": msg, "verified_with_trust_anchor": trust.is_some()}),
                );
                let _ = self
                    .in_tx(move |s| {
                        ev.append(s, now)?;
                        Ok(())
                    })
                    .await;
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    code,
                    format!("無法匯入：{msg}"),
                )
                .with_product_code(CODE_IMPORT_FAILED));
            }
        };
        if verified.signer.is_none() {
            log::warn!(
                "遠端管理：remote.allowed_signers 沒有設定，Pulse {} 匯入時沒有驗證簽署者（金鑰 {}；每台 Pulse 仍以本機的信任錨驗證）",
                verified.version,
                verified.key_fingerprint
            );
        }
        let who = who.clone();
        let v = verified.clone();
        let (row, artifacts) = self
            .in_tx(move |s| {
                if s.get_release(&v.version)?.is_some() {
                    return Err(ApiError::conflict(
                        "release_exists",
                        format!(
                            "版本庫已有 Pulse {}；要補上發佈檔請用 PUT …/artifacts/{{filename}}",
                            v.version
                        ),
                    )
                    .into());
                }
                let mut row = ReleaseRow {
                    id: 0,
                    version: v.version.clone(),
                    commit: v.commit.clone(),
                    source: if base_url.is_some() {
                        SOURCE_EXTERNAL
                    } else {
                        SOURCE_HOSTED
                    }
                    .into(),
                    base_url: base_url.clone(),
                    sha256sums: sha256sums.clone(),
                    signature: signature.clone(),
                    signer_fingerprint: v.signer.as_ref().map(|s| s.1.clone()),
                    signer_principal: v.signer.as_ref().map(|s| s.0.clone()),
                    manifest_name: v.manifest_name.clone(),
                    manifest: manifest.clone(),
                    imported_at: now,
                    imported_by: who.actor.clone(),
                };
                row.id = s.insert_release(&row)?;
                let mut artifacts = Vec::new();
                for a in &v.artifacts {
                    let a = ArtifactRow {
                        release_id: row.id,
                        ..a.clone()
                    };
                    s.upsert_artifact(&a)?;
                    artifacts.push(a);
                }
                who.event(
                    "release",
                    "release.import",
                    Some(format!("pulse {}", v.version)),
                    None,
                    json!({
                        "version": v.version,
                        "commit": v.commit,
                        "files": v.artifacts.iter().map(|a| a.filename.clone()).collect::<Vec<_>>(),
                        "signer_fingerprint": v.signer.as_ref().map(|s| s.1.clone()),
                        "signer_principal": v.signer.as_ref().map(|s| s.0.clone()),
                        "key_fingerprint": v.key_fingerprint,
                        "verified_with_trust_anchor": v.signer.is_some(),
                        "source": row.source,
                        "base_url": row.base_url,
                    }),
                )
                .append(s, now)?;
                Ok((row, artifacts))
            })
            .await
            .map_err(|e| e.into_api("發佈版本"))?;
        self.forget_recommended_min();
        match &row.base_url {
            Some(u) => {
                log::info!(event = "release.import"; "版本庫：已匯入 Pulse {}（外部 URL {u}，{} 個發佈檔由 Pulse 自行下載）", row.version, artifacts.len())
            }
            None => {
                log::info!(event = "release.import"; "版本庫：已匯入 Pulse {}（{} 個發佈檔待上傳）", row.version, artifacts.len())
            }
        }
        Ok(release_json(&row, &artifacts, None))
    }

    /// `PUT /api/v1/releases/{version}/artifacts/{filename}`：上傳完成後檢查並存入版本庫
    pub async fn store_artifact(
        &self,
        who: &Actor,
        version: String,
        filename: String,
        upload: Upload,
        sha256: String,
        size: u64,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let (version2, filename2) = (version.clone(), filename.clone());
        let w = who.clone();
        let res = self
            .in_tx(move |s| {
                let who = &w;
                let rel = s
                    .get_release(&version)?
                    .ok_or_else(|| not_found("release_not_found", format!("版本庫沒有 Pulse {version}")))?;
                let artifacts = s.release_artifacts(rel.id)?;
                let Some(a) = artifacts.iter().find(|a| a.filename == filename) else {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "artifact_not_listed",
                        format!("{filename} 不是 Pulse {version} 的 SHA256SUMS 中列出的發佈檔"),
                    )
                    .with_product_code(CODE_IMPORT_FAILED)
                    .into());
                };
                if a.sha256 != sha256 {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "artifact_hash_mismatch",
                        format!("{filename} 的 SHA-256 與 SHA256SUMS 不符（檔案損毀或不是同一次發佈）"),
                    )
                    .with_product_code(CODE_IMPORT_FAILED)
                    .with_details(json!({"expected": a.sha256, "actual": sha256}))
                    .into());
                }
                // 外部 URL 的版本庫：只「登記」——確認雜湊相符並記下大小（提議的 artifact.size 需要它，
                // SHA256SUMS 沒有大小），檔案不存入版本庫（upload 在離開時刪除暫存檔）
                let external = rel.source == SOURCE_EXTERNAL;
                if external {
                    drop(upload);
                } else {
                    upload
                        .commit(&sha256)
                        .map_err(|e| DbError(format!("無法寫入版本庫：{e}")))?;
                }
                let row = ArtifactRow {
                    size: size as i64,
                    stored: !external,
                    stored_at: (!external).then_some(now),
                    ..a.clone()
                };
                s.upsert_artifact(&row)?;
                who.event(
                    "release",
                    "release.import",
                    Some(format!("pulse {version}")),
                    None,
                    json!({"version": version, "artifact": filename, "sha256": sha256, "size": size, "registered_only": external}),
                )
                .append(s, now)?;
                Ok(row)
            })
            .await;
        match res {
            Ok(row) => Ok(artifact_json(&row)),
            Err(TxError::Api(e)) => {
                if e.product_code == Some(CODE_IMPORT_FAILED) {
                    log::warn!(event = "release.import_failed", code = CODE_IMPORT_FAILED; "錯誤 {CODE_IMPORT_FAILED}：{}", e.message);
                    let ev = who.event(
                        "release",
                        "release.import_failed",
                        Some(format!("pulse {version2}")),
                        Some(CODE_IMPORT_FAILED),
                        json!({"reason": e.code, "artifact": filename2, "message": e.message}),
                    );
                    let _ = self
                        .in_tx(move |s| {
                            ev.append(s, now)?;
                            Ok(())
                        })
                        .await;
                }
                Err(e)
            }
            Err(e) => Err(e.into_api("發佈檔")),
        }
    }

    /// `DELETE /api/v1/releases/{version}`（L2：`confirm` 為版本號）
    pub async fn delete_release(
        &self,
        who: &Actor,
        version: String,
        reason: Option<String>,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let files = self
            .in_tx(move |s| {
                let rel = s
                    .get_release(&version)?
                    .ok_or_else(|| not_found("release_not_found", format!("版本庫沒有 Pulse {version}")))?;
                let rollouts = s.rollouts_using_release(rel.id)?;
                let artifacts = s.release_artifacts(rel.id)?;
                let rollback_for = rollback_target_count(&s.upgrade_hosts()?, &rel.version, &artifacts);
                if rollouts > 0 || rollback_for > 0 {
                    return Err(ApiError::conflict(
                        "release_in_use",
                        format!(
                            "Pulse {version} 仍在使用中：{rollouts} 個推送以它為目標、{rollback_for} 台主機以它為回滾目標"
                        ),
                    )
                    .with_details(json!({"in_use_by_rollouts": rollouts, "rollback_target_for": rollback_for}))
                    .into());
                }
                s.delete_release(rel.id)?;
                let mut gone = Vec::new();
                for a in artifacts.iter().filter(|a| a.stored) {
                    if s.artifact_refs(&a.sha256)? == 0 {
                        gone.push(a.sha256.clone());
                    }
                }
                who.event(
                    "release",
                    "release.delete",
                    Some(format!("pulse {version}")),
                    None,
                    json!({"version": version, "files": artifacts.iter().map(|a| a.filename.clone()).collect::<Vec<_>>(), "reason": reason}),
                )
                .append(s, now)?;
                Ok(gone)
            })
            .await
            .map_err(|e| e.into_api("發佈版本"))?;
        self.forget_recommended_min();
        for sha in files {
            let p = releases::artifact_path(&self.releases_dir, &sha);
            if let Err(e) = std::fs::remove_file(&p)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("版本庫：無法刪除 {}：{e}", p.display());
            }
        }
        Ok(json!({"deleted": true}))
    }
}

/// `POST /api/v1/config/preview` 的假設變更
#[derive(Debug, Clone)]
pub enum PreviewChange {
    Profile {
        id: i64,
        settings: Map<String, Value>,
    },
    Assign {
        profile_id: i64,
        selector: Selector,
        priority: i64,
        expires_at: Option<i64>,
    },
    Unassign {
        id: i64,
    },
}

/// 以這個版本為回滾目標的主機數（deb／rpm、目前執行這個版本、且版本庫有同格式同平台的檔案）
pub fn rollback_target_count(
    hosts: &[super::store::UpgradeHost],
    version: &str,
    artifacts: &[ArtifactRow],
) -> usize {
    hosts
        .iter()
        .filter(|h| h.agent_version.as_deref() == Some(version))
        .filter(|h| {
            let method = h.install_method.as_deref().unwrap_or("");
            matches!(method, "deb" | "rpm")
                && super::rollout::pick_artifact(
                    artifacts,
                    method,
                    &super::rollout::arch_of(h.os.as_deref()).unwrap_or_default(),
                )
                .is_some()
        })
        .count()
}

/// 發佈檔的 JSON
pub fn artifact_json(a: &ArtifactRow) -> Value {
    json!({
        "filename": a.filename,
        "os": a.os,
        "arch": a.arch,
        "format": a.format,
        "sha256": a.sha256,
        "size": (a.size > 0).then_some(a.size),
        "stored": a.stored,
        // 外部 URL 的版本庫：已登記（雜湊確認過、大小已知，可以推送），檔案不在 Kairos
        "registered": !a.stored && a.size > 0,
        "stored_at": a.stored_at.map(crate::api::http::ts_ms),
    })
}

/// 在 [`release_json`] 的 `artifacts[]` 加上 `file_problem`：已存入（`stored`）的發佈檔在這個節點的
/// 版本庫中不存在（`"missing"`）或大小與記錄不符（`"size_mismatch"`），正常與未存入的為 `null`
/// （[`releases::file_problem`]，與系統健康「版本庫的檔案遺失」相同的判斷，只 `stat`）。主控台據此在
/// 重新匯入時補上檔案，而不是把它當成「已上傳過」跳過
pub fn annotate_file_problems(v: &mut Value, artifacts: &[ArtifactRow], dir: &std::path::Path) {
    if let Some(list) = v["artifacts"].as_array_mut() {
        for (j, a) in list.iter_mut().zip(artifacts) {
            j["file_problem"] = json!(
                a.stored
                    .then(|| releases::file_problem(dir, &a.sha256, a.size))
                    .flatten()
            );
        }
    }
}

/// `releases.source`
pub const SOURCE_HOSTED: &str = "hosted";
/// 外部 URL 的版本庫（REMOTE.md 第 4.1.2 節）
pub const SOURCE_EXTERNAL: &str = "external";

/// 推送可以用的發佈檔：代管的版本為已存入的檔案；外部 URL 的版本為**已登記**的檔案（上傳或
/// `kairos releases import --external` 時確認過雜湊並記下大小；提議的 `artifact.size` 需要它。Kairos 無法
/// 得知 URL 上是否真的有檔案，由 Pulse 下載時發現）。回傳的 `stored` 表示「可以提議」
pub fn offerable(rel: &ReleaseRow, mut artifacts: Vec<ArtifactRow>) -> Vec<ArtifactRow> {
    if rel.source == SOURCE_EXTERNAL {
        for a in &mut artifacts {
            a.stored = a.size > 0;
        }
    }
    artifacts
}

/// 版本的 JSON（`usage`：(目前執行的主機數, 作為回滾目標的主機數, 使用中的推送數)）
pub fn release_json(
    r: &ReleaseRow,
    artifacts: &[ArtifactRow],
    usage: Option<(usize, usize, i64)>,
) -> Value {
    let manifest = r
        .manifest
        .as_deref()
        .and_then(|m| kairos_proto::release::ReleaseManifest::parse(m).ok());
    let mut v = json!({
        "version": r.version,
        "commit": r.commit,
        "source": r.source,
        "base_url": r.base_url,
        "signer_fingerprint": r.signer_fingerprint,
        "signer_principal": r.signer_principal,
        "verified": r.signer_fingerprint.is_some(),
        "manifest": manifest.as_ref().map(|m| json!({
            "filename": r.manifest_name,
            "released_at": m.released_at,
            "min_kairos": m.min_kairos,
            "min_upgrade_from": m.min_upgrade_from,
            "remote_features": m.remote_features,
            "breaking": m.breaking,
            "kairos_compatible": kairos_proto::release::Version::parse(crate::VERSION).is_ok_and(|k| m.kairos_compatible(&k)),
        })),
        "artifacts": artifacts.iter().map(artifact_json).collect::<Vec<_>>(),
        "imported_at": crate::api::http::ts_ms(r.imported_at),
        "imported_by": r.imported_by,
    });
    if let Some((on, rollback, rollouts)) = usage {
        v["hosts_on_version"] = json!(on);
        v["rollback_target_for"] = json!(rollback);
        v["in_use_by_rollouts"] = json!(rollouts);
    }
    v
}

/// 建議的最低 Pulse 版本：版本庫中最新版本的清單的 `min_upgrade_from`（REMOTE.md 第 4.9 節；沒有版本、
/// 最新版本沒有清單或清單無法解析時 `None`）
pub fn recommended_min_version(releases: &[ReleaseRow]) -> Option<String> {
    use kairos_proto::release::{ReleaseManifest, Version};
    let latest = releases
        .iter()
        .filter_map(|r| Version::parse(&r.version).ok().map(|v| (v, r)))
        .max_by(|a, b| a.0.cmp(&b.0))?
        .1;
    let m = ReleaseManifest::parse(latest.manifest.as_deref()?).ok()?;
    Version::parse(&m.min_upgrade_from).ok()?;
    Some(m.min_upgrade_from)
}
