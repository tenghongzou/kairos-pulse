//! 升級推送的操作（主控台 API）與推送引擎：建立、送出、暫停、繼續、取消、重試；每一輪的逾時、提議與批次
//! （[`super::rollout::plan_tick`]）；Pulse 回報的 `upgrade_status`；組出 `upgrade_offer`。

use hyper::StatusCode;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use kairos_proto::mgmt::{
    self, ACTION_DOWNGRADE, ACTION_UPGRADE, ApplyWindow, ArtifactRef, ManifestRef, SOURCE_KAIROS,
    UpgradeOffer, UpgradeStatus, UpgradeTarget,
};
use kairos_proto::release::{ReleaseManifest, Version};

use super::ops::{Actor, TxError};
use super::rollout::{
    self, HOST_ACCEPTED, HOST_APPLYING, HOST_CANCELLED, HOST_DECLINED, HOST_EXPIRED, HOST_FAILED,
    HOST_OFFERED, HOST_PENDING, HOST_ROLLBACK_FAILED, HOST_ROLLED_BACK, HOST_SKIPPED,
    HOST_SKIPPED_OFFLINE, HOST_STAGED, HOST_SUCCEEDED, HOST_UNKNOWN, HostAction, ROLLOUT_CANCELLED,
    ROLLOUT_COMPLETED, ROLLOUT_DRAFT, ROLLOUT_PAUSED, ROLLOUT_PENDING_APPROVAL, ROLLOUT_RUNNING,
    ROLLOUT_SCHEDULED, ReleaseContext, RolloutAction, RolloutSpec,
};
use super::schedule::ScheduleSpec;
use super::store::{HostTransition, NewRolloutHost, RolloutHostRow, RolloutRow};
use super::submission::{self, MissingFile};
use super::{
    CODE_INCOMPATIBLE, CODE_RELEASE_FILE_MISSING, CODE_ROLLBACK_FAILED, CODE_ROLLED_BACK,
    CODE_ROLLOUT_PAUSED, CODE_SIGNATURE_FAILED, CODE_VERSION_MISMATCH, OfferFiles, RemoteService,
};
use crate::api::http::ApiError;
use crate::enroll::db::DbError;
use crate::store::db::Store;

/// `POST /api/v1/rollouts/{id}/approve` 的本文（ADR-0002 決策點 2b）：`confirm` 與 `digest` 必填
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApproveRequest {
    /// 「核准 N 台」／「核准降版 N 台」（推送的 `submission.confirm_text`）
    pub confirm: Option<String>,
    /// 核准者檢視的那一份送審內容（推送的 `submission.digest`）
    pub digest: Option<String>,
    /// 降版時必填
    pub reason: Option<String>,
}

/// 新的 upgrade_id（UUID v4）
pub fn new_upgrade_id() -> Result<String, DbError> {
    let mut b = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b)
        .map_err(|_| DbError("無法取得隨機數".into()))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "rollout_not_found",
        format!("沒有推送 {id}"),
    )
}

fn invalid_state(r: &RolloutRow, what: &str) -> ApiError {
    ApiError::conflict(
        "invalid_state",
        format!("推送 {} 目前是 {}，不能{what}", r.id, r.state),
    )
    .with_details(json!({"state": r.state}))
}

/// 目標版本與版本庫（資格分類用）。`missing`：檔案有問題的發佈檔（[`submission::missing_files`]）——它們不算
/// 可以提議的檔案（ADR-0002 F1：需要它的主機分類為 `artifact_missing`；回滾檔遺失的主機算無法自動回滾）
pub(super) fn release_context(
    s: &dyn Store,
    version: &str,
    missing: &[MissingFile],
) -> Result<(i64, ReleaseContext), TxError> {
    let rel = s.get_release(version)?.ok_or_else(|| {
        TxError::Api(ApiError::param(
            "target_version",
            format!("版本庫沒有 Pulse {version}（先匯入發佈版本）"),
        ))
    })?;
    let target = Version::parse(&rel.version)
        .map_err(|e| TxError::Api(ApiError::param("target_version", e)))?;
    let manifest = rel
        .manifest
        .as_deref()
        .and_then(|m| ReleaseManifest::parse(m).ok());
    // 外部 URL 的版本（0.10）：SHA256SUMS 列出的發佈檔都可以提議。回滾檔來自外部 URL 的版本時，只在目標
    // 也是外部 URL 時才附上（主機本來就要允許外部 URL；否則一台不允許外部 URL 的主機會因為回滾檔而拒絕
    // 整個提議），見 [`RemoteService::build_offer`]
    let external = rel.source == super::ops::SOURCE_EXTERNAL;
    let mut by_version: HashMap<String, Vec<super::store::ArtifactRow>> = HashMap::new();
    let releases: HashMap<i64, super::store::ReleaseRow> =
        s.list_releases()?.into_iter().map(|r| (r.id, r)).collect();
    let mut missing_target = Vec::new();
    for a in s.all_artifacts()? {
        if let Some(r) = releases.get(&a.release_id) {
            if r.source == super::ops::SOURCE_EXTERNAL && !external {
                continue;
            }
            if missing
                .iter()
                .any(|m| m.artifact.release_id == a.release_id && m.artifact.filename == a.filename)
            {
                if r.id == rel.id {
                    missing_target.push(a);
                }
                continue;
            }
            by_version
                .entry(r.version.clone())
                .or_default()
                .extend(super::ops::offerable(r, vec![a]));
        }
    }
    let artifacts = by_version.get(&rel.version).cloned().unwrap_or_default();
    Ok((
        rel.id,
        ReleaseContext {
            target,
            manifest,
            artifacts,
            by_version,
            missing: missing_target,
        },
    ))
}

/// `min_kairos` 檢查（REMOTE.md 第 4.9 節）
fn incompatible(ctx: &ReleaseContext) -> Option<String> {
    let m = ctx.manifest.as_ref()?;
    let k = Version::parse(crate::VERSION).ok()?;
    (!m.kairos_compatible(&k)).then(|| m.min_kairos.clone())
}

fn spec_of(
    r: &RolloutRow,
) -> (
    rollout::SelectorSpec,
    Vec<rollout::WaveSpec>,
    rollout::RolloutOptions,
) {
    (
        rollout::selector_from_db(&r.selector),
        rollout::waves_from_db(&r.waves),
        rollout::options_from_db(&r.options),
    )
}

/// 儲存用的排程：有時段而沒有指定 `timezone` 時填入預設（`alerts.timezone`）；`ends_at` 已過時拒絕
fn schedule_to_store(
    spec: &RolloutSpec,
    default_tz: &str,
    now: i64,
) -> Result<ScheduleSpec, ApiError> {
    let mut schedule = spec.schedule.clone();
    if schedule.ends_at.is_some_and(|e| e <= now) {
        return Err(ApiError::param(
            "schedule.ends_at",
            "ends_at 已經過了".to_string(),
        ));
    }
    if schedule.timezone.is_none() && !schedule.windows.is_empty() {
        schedule.timezone = Some(default_tz.to_string());
    }
    Ok(schedule)
}

/// 推送的 `next_action`（[`rollout::next_action_scheduled`]）；進行中而有主機在等版本庫的檔案補回時為
/// `waiting_artifact{hosts}`（ADR-0002 F3：不計入失敗、不自動暫停，補檔後下一輪自動繼續）
pub(crate) fn next_action_of(r: &RolloutRow, hosts: &[RolloutHostRow], now_ms: i64) -> Value {
    next_action_summary(r, &HostSummary::of(hosts), now_ms)
}

/// 推送的主機統計（清單與詳情的 `progress`、`hosts_total`、`rollback_failed_unacknowledged` 與
/// `next_action` 的 `waiting_artifact` 需要的數字）。詳情從主機列算（[`HostSummary::of`]）；清單由
/// 資料庫彙總（[`HostSummary::from_counts`]，`RemoteStore::rollout_host_counts`；0.10.1），兩者的結果相同
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSummary {
    /// 各狀態的台數：15 個已知的狀態都有鍵（沒有主機時為 0）
    pub progress: BTreeMap<String, usize>,
    /// 回滾失敗、尚未標記為已處理的台數
    pub rollback_failed_unacknowledged: usize,
    /// 等待版本庫檔案的台數（`pending` 且 `reason = artifact_missing`）
    pub waiting_artifact: usize,
}

impl Default for HostSummary {
    fn default() -> Self {
        Self::of(&[])
    }
}

impl HostSummary {
    pub fn of(hosts: &[RolloutHostRow]) -> Self {
        Self {
            progress: rollout::progress(hosts),
            rollback_failed_unacknowledged: hosts
                .iter()
                .filter(|h| h.state == HOST_ROLLBACK_FAILED && h.acknowledged_at.is_none())
                .count(),
            waiting_artifact: hosts
                .iter()
                .filter(|h| {
                    h.state == HOST_PENDING
                        && h.reason.as_deref() == Some(rollout::SKIP_ARTIFACT_MISSING)
                })
                .count(),
        }
    }

    /// 資料庫彙總的列（所有推送）依推送分組
    pub fn from_counts(rows: Vec<super::store::RolloutHostCount>) -> HashMap<String, HostSummary> {
        let mut out: HashMap<String, HostSummary> = HashMap::new();
        for c in rows {
            let s = out.entry(c.rollout_id).or_default();
            let (n, flagged) = (
                usize::try_from(c.hosts).unwrap_or(0),
                usize::try_from(c.flagged).unwrap_or(0),
            );
            if c.state == HOST_ROLLBACK_FAILED {
                s.rollback_failed_unacknowledged += flagged;
            } else if c.state == HOST_PENDING {
                s.waiting_artifact += flagged;
            }
            *s.progress.entry(c.state).or_default() += n;
        }
        out
    }

    /// 不含 `skipped` 的台數（API 的 `hosts_total`）
    pub fn hosts_total(&self) -> usize {
        self.progress
            .iter()
            .filter(|(state, _)| state.as_str() != HOST_SKIPPED)
            .map(|(_, n)| n)
            .sum()
    }
}

fn next_action_summary(r: &RolloutRow, hosts: &HostSummary, now_ms: i64) -> Value {
    let v = rollout::next_action_scheduled(
        &r.state,
        r.soak_until,
        r.awaiting_continue,
        r.pause_reason.as_deref(),
        r.pause_cause.as_deref(),
        &ScheduleSpec::from_db(&r.schedule),
        now_ms,
    );
    if v["code"] == "upgrading" {
        let waiting = hosts.waiting_artifact;
        if waiting > 0 {
            return json!({"code": "waiting_artifact", "hosts": waiting});
        }
    }
    v
}

/// 推送開始（或排定開始）時有沒有經過另一位管理員核准（API 的 `approval`；0.10.1，REMOTE.md 第 5.3 節）。
/// 由既有欄位導出，不另存：推送離開草稿只有 `submit` 一條路，雙人覆核為 `required` 時一定先到
/// `pending_approval`、核准時寫入 `approved_at`；為 `none` 時直接排程或開始，`approved_at` 維持 `None`
/// （每次送出都會清除上一輪的核准）。
///
/// | 值 | 意義 |
/// |---|---|
/// | `approved` | 有核准紀錄（`approved_at`／`approved_by`） |
/// | `not_required` | 送出時雙人覆核是 `none`：沒有經過核准就排程或開始 |
/// | `unknown` | 沒有送出紀錄（`submitted_at` 是 schema v10 加的：0.9.x 建立並開始的推送），無法判斷 |
/// | `None` | 還沒有開始也沒有排定（草稿、待核准、開始之前就取消） |
///
/// 這是資料庫目前的內容，能寫資料庫的人可以改；不可否認的紀錄在稽核雜湊鏈（`rollout.submit`、
/// `rollout.start` 的 `details.rollout_approval`／`approval`）。
pub fn approval_of(r: &RolloutRow) -> Option<&'static str> {
    if r.state == ROLLOUT_DRAFT || r.state == ROLLOUT_PENDING_APPROVAL {
        return None;
    }
    if r.started_at.is_none() && r.state != ROLLOUT_SCHEDULED {
        return None;
    }
    Some(if r.approved_at.is_some() {
        APPROVAL_APPROVED
    } else if r.submitted_at.is_some() {
        APPROVAL_NOT_REQUIRED
    } else {
        APPROVAL_UNKNOWN
    })
}

/// 稽核 `rollout.start` 的 `details`（0.10.1）：推送實際開始的那一刻留在雜湊鏈裡的證據——當下生效的
/// 雙人覆核政策（`rollout_approval`）、這個推送有沒有經過核准（`approval`）、送出者與核准者。
/// 即使 `meta` 的政策前值或 `rollouts` 的列之後被改掉，這一筆仍看得出「政策為 `none` 時開始」。
/// `extra` 是各入口原有的欄位（`approved`、`scheduled`）
fn start_details(r: &RolloutRow, policy: &str, extra: Value) -> Value {
    let mut v = json!({
        "rollout_id": r.id,
        "name": r.name,
        "target_version": r.target_version,
        "rollout_approval": policy,
        "approval": approval_of(r),
        "submitted_by": r.submitted_by,
        "approved_by": r.approved_by,
    });
    if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
        m.extend(e);
    }
    v
}

/// 沒有經過核准就開始的推送在日誌留一行（警告）
fn warn_unapproved_start(r: &RolloutRow) {
    if approval_of(r) == Some(APPROVAL_NOT_REQUIRED) {
        log::warn!(
            event = "rollout.start_unapproved";
            "推送「{}」（{}）沒有經過另一位管理員核准就開始（送出時 remote.rollout_approval = none；送出者 {}）",
            r.name,
            r.id,
            r.submitted_by.as_deref().unwrap_or("—")
        );
    }
}

/// [`approval_of`] 的值
pub const APPROVAL_APPROVED: &str = "approved";
pub const APPROVAL_NOT_REQUIRED: &str = "not_required";
pub const APPROVAL_UNKNOWN: &str = "unknown";

/// 推送的 JSON（清單與詳情；`hosts` 有給時另加各批的統計）
pub fn rollout_json(r: &RolloutRow, hosts: &[RolloutHostRow], now_ms: i64, detail: bool) -> Value {
    let mut v = rollout_summary_json(r, &HostSummary::of(hosts), now_ms);
    if detail {
        let (selector, waves, options) = spec_of(r);
        v["selector"] = serde_json::to_value(&selector).unwrap_or(Value::Null);
        v["options"] = serde_json::to_value(&options).unwrap_or(Value::Null);
        v["waves"] = rollout::waves_json(&waves, hosts, r.current_wave);
        v["schedule"] = ScheduleSpec::from_db(&r.schedule).json();
    }
    v
}

/// 推送的 JSON 中不需要逐台主機的部分（清單的每一筆；0.10.1 起清單的主機統計由資料庫彙總）
pub fn rollout_summary_json(r: &RolloutRow, hosts: &HostSummary, now_ms: i64) -> Value {
    let (_, waves, _) = spec_of(r);
    let schedule = ScheduleSpec::from_db(&r.schedule);
    let ts = |v: Option<i64>| v.map(crate::api::http::ts_ms);
    json!({
        "id": r.id,
        "name": r.name,
        "target_version": r.target_version,
        "state": r.state,
        "current_wave": r.current_wave,
        "waves_total": waves.len(),
        "progress": hosts.progress,
        "hosts_total": hosts.hosts_total(),
        // 回滾失敗、尚未標記為已處理的主機（progress.rollback_failed 含已處理的）
        "rollback_failed_unacknowledged": hosts.rollback_failed_unacknowledged,
        "next_action": next_action_summary(r, hosts, now_ms),
        "starts_at": ts(schedule.starts_at),
        "ends_at": ts(schedule.ends_at),
        "pause_reason": r.pause_reason,
        "pause_cause": rollout::pause_cause(&r.state, r.pause_cause.as_deref()),
        "cancel_reason": r.cancel_reason,
        "awaiting_continue": r.awaiting_continue,
        "soak_until": ts(r.soak_until),
        "created_at": crate::api::http::ts_ms(r.created_at),
        "created_by": r.created_by,
        "submitted_at": ts(r.submitted_at),
        "submitted_by": r.submitted_by,
        "approved_at": ts(r.approved_at),
        "approved_by": r.approved_by,
        // 開始（或排定開始）時有沒有經過核准：approved／not_required／unknown／null（approval_of）
        "approval": approval_of(r),
        "reject_reason": r.reject_reason,
        "started_at": ts(r.started_at),
        "completed_at": ts(r.completed_at),
        "updated_at": crate::api::http::ts_ms(r.updated_at),
    })
}

/// 暫停（主控台或自動）：提議中尚未替換的主機回到 `pending`（恢復時以新的 upgrade_id 重新提議）。
/// 回傳不再提議的主機
fn pause_in_tx(
    s: &dyn Store,
    r: &mut RolloutRow,
    reason: &str,
    who: &Actor,
    auto: Option<Value>,
    now: i64,
) -> Result<Vec<String>, TxError> {
    let hosts = s.rollout_hosts(&r.id)?;
    let mut cleared = Vec::new();
    for h in hosts.iter().filter(|h| rollout::is_offered(&h.state)) {
        s.update_rollout_host(
            &r.id,
            h.host_id,
            &HostTransition {
                state: Some(HOST_PENDING.into()),
                upgrade_id: Some(new_upgrade_id()?),
                offered_at: Some(None),
                ..Default::default()
            },
            now,
        )?;
        cleared.push(h.agent_id.clone());
    }
    r.state = ROLLOUT_PAUSED.into();
    r.pause_reason = Some(reason.to_string());
    r.pause_cause = auto
        .as_ref()
        .and_then(|d| d.get("cause"))
        .and_then(Value::as_str)
        .map(str::to_string);
    r.updated_at = now;
    s.update_rollout_state(r)?;
    let (action, code) = if auto.is_some() {
        ("rollout.auto_pause", Some(CODE_ROLLOUT_PAUSED))
    } else {
        ("rollout.pause", None)
    };
    who.event(
        "rollout",
        action,
        Some(r.id.clone()),
        code,
        json!({"rollout_id": r.id, "name": r.name, "reason": reason, "details": auto, "progress": rollout::progress(&hosts)}),
    )
    .append(s, now)?;
    if auto.is_some() {
        log::warn!(
            event = "rollout.auto_pause", code = CODE_ROLLOUT_PAUSED;
            "警告 {CODE_ROLLOUT_PAUSED}：推送「{}」（{}）已自動暫停：{reason}",
            r.name,
            r.id
        );
    }
    Ok(cleared)
}

/// 推送結束時的 `rollout.complete`（各狀態的台數）
fn complete_in_tx(s: &dyn Store, r: &mut RolloutRow, now: i64) -> Result<(), TxError> {
    let hosts = s.rollout_hosts(&r.id)?;
    r.state = ROLLOUT_COMPLETED.into();
    r.completed_at = Some(now);
    r.soak_until = None;
    r.awaiting_continue = false;
    r.updated_at = now;
    s.update_rollout_state(r)?;
    Actor::system()
        .event(
            "rollout",
            "rollout.complete",
            Some(r.id.clone()),
            None,
            json!({"rollout_id": r.id, "name": r.name, "target_version": r.target_version, "progress": rollout::progress(&hosts)}),
        )
        .append(s, now)?;
    log::info!(event = "rollout.complete"; "推送「{}」（{}）已完成", r.name, r.id);
    Ok(())
}

/// 下一批（或完成）
fn next_wave_in_tx(
    s: &dyn Store,
    r: &mut RolloutRow,
    waves: usize,
    now: i64,
) -> Result<(), TxError> {
    if r.current_wave as usize + 1 >= waves {
        return complete_in_tx(s, r, now);
    }
    r.current_wave += 1;
    r.wave_started_at = Some(now);
    r.soak_until = None;
    r.awaiting_continue = false;
    r.updated_at = now;
    s.update_rollout_state(r)?;
    log::info!(event = "rollout.wave"; "推送「{}」（{}）開始第 {} 批", r.name, r.id, r.current_wave + 1);
    Ok(())
}

impl RemoteService {
    // ---- 主控台的操作 ----

    /// `POST /api/v1/rollouts/preview`
    pub async fn preview_rollout(
        &self,
        spec: RolloutSpec,
        rollout_id: Option<String>,
        viewer: Option<String>,
    ) -> Result<Value, ApiError> {
        let live = self.live_hellos();
        let now = self.now_ms();
        let default_tz = self.default_timezone();
        let approval = self.settings.approval_required();
        let dir = self.releases_dir.clone();
        self.in_tx(move |s| {
            let missing = submission::missing_files(s, &dir)?;
            let (_, ctx) = release_context(s, &spec.target_version, &missing)?;
            let busy = rollout::busy_map(s.hosts_in_active_rollouts()?);
            let id = rollout_id.unwrap_or_else(|| "preview".to_string());
            let (p, _, _) = rollout::plan(
                &id,
                &with_live(s.upgrade_hosts()?, &live),
                &spec.selector,
                &spec.waves,
                &spec.options,
                &ctx,
                &busy,
            );
            let mut v = p.json();
            v["incompatible"] = json!(
                incompatible(&ctx).map(|min| json!({"min_kairos": min, "code": CODE_INCOMPATIBLE}))
            );
            v["breaking"] = json!(ctx.manifest.as_ref().is_some_and(|m| m.breaking));
            v["remote_features"] = json!(ctx.manifest.as_ref().map(|m| m.remote_features.clone()));
            v["loses_remote_management"] = json!(
                ctx.manifest
                    .as_ref()
                    .is_some_and(|m| !m.supports_remote_upgrade())
                    || ctx.manifest.is_none()
            );
            v["confirm_text"] = json!(rollout::confirm_text(
                p.eligible,
                p.downgrade > 0,
                p.no_rollback
            ));
            v["confirm_level"] = json!(if p.downgrade > 0 { "L3" } else { "L2" });
            v["waves_spec"] = spec.waves_json();
            // 排程：正規化後的內容與目前是否在時段內（主控台顯示「送出後等待時段」）
            let mut schedule = spec.schedule.clone();
            if schedule.timezone.is_none() && !schedule.windows.is_empty() {
                schedule.timezone = Some(default_tz.clone());
            }
            v["schedule"] = schedule.json();
            v["window"] = match schedule.window_at(now) {
                super::schedule::WindowState::Open { until } => {
                    json!({"open": true, "until": until.map(crate::api::http::ts_ms)})
                }
                super::schedule::WindowState::Closed { next } => {
                    json!({"open": false, "next": next.map(crate::api::http::ts_ms)})
                }
            };
            v["default_timezone"] = json!(default_tz);
            let template = |w: Vec<rollout::WaveSpec>| {
                Value::Array(w.iter().map(rollout::WaveSpec::json).collect())
            };
            v["wave_templates"] = json!({
                "default": template(rollout::WaveSpec::default_template()),
                "recommended": template(rollout::WaveSpec::recommended_template()),
            });
            v["max_waves"] = json!(rollout::MAX_WAVES);
            // 雙人覆核：送出後要等另一位使用者核准（主控台在送出確認中說明）
            v["approval_required"] = json!(approval);
            // 目標版本中檔案有問題的發佈檔（需要它的主機已分類為 artifact_missing）與風險提醒的固定代碼
            v["artifact_problems"] = Value::Array(
                missing
                    .iter()
                    .filter(|m| {
                        ctx.missing.iter().any(|a| {
                            a.filename == m.artifact.filename
                                && a.release_id == m.artifact.release_id
                        })
                    })
                    .map(|m| json!({"version": m.version, "filename": m.artifact.filename, "file_problem": m.problem}))
                    .collect(),
            );
            let mut warnings = rollout::risk_warnings(
                &spec.waves,
                &p.waves,
                p.eligible,
                p.downgrade,
                p.no_rollback,
                !ctx.missing.is_empty(),
            );
            // 沒有可核准者的偵測（決策點 2g）：以這次請求的身分為建立者與送出者計算；沒有啟用雙人覆核、
            // 或不知道身分時為 null
            v["approvers_available"] = match (&viewer, approval) {
                (Some(actor), true) => {
                    let n = submission::approvers_available(
                        &submission::approvers(s, now)?,
                        actor,
                        Some(actor.as_str()),
                    );
                    if n == 0 {
                        warnings.push("no_approvers");
                    }
                    json!(n)
                }
                _ => Value::Null,
            };
            v["warnings"] = json!(warnings);
            Ok(v)
        })
        .await
        .map_err(|e| e.into_api("推送預覽"))
    }

    /// `POST /api/v1/rollouts`：建立草稿
    pub async fn create_rollout(&self, who: &Actor, spec: RolloutSpec) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let live = self.live_hellos();
        let schedule = schedule_to_store(&spec, &self.default_timezone(), now)?;
        let dir = self.releases_dir.clone();
        self.in_tx(move |s| {
            let missing = submission::missing_files(s, &dir)?;
            let (release_id, ctx) = release_context(s, &spec.target_version, &missing)?;
            if let Some(min) = incompatible(&ctx) {
                return Err(incompatible_error(&spec.target_version, &min).into());
            }
            let busy = rollout::busy_map(s.hosts_in_active_rollouts()?);
            let (p, _, _) = rollout::plan("draft", &with_live(s.upgrade_hosts()?, &live), &spec.selector, &spec.waves, &spec.options, &ctx, &busy);
            if p.overlap_count > 0 {
                return Err(overlap_error(&p).into());
            }
            let day = chrono::DateTime::from_timestamp_millis(now).unwrap_or_default().format("%Y%m%d").to_string();
            let prefix = format!("r-{day}-");
            let n = s.count_rollouts_with_prefix(&prefix)? + 1;
            let r = RolloutRow {
                id: format!("{prefix}{n:04}"),
                name: spec.name.clone(),
                release_id,
                target_version: spec.target_version.clone(),
                selector: serde_json::to_string(&spec.selector).unwrap_or_default(),
                waves: spec.waves_json().to_string(),
                schedule: schedule.json().to_string(),
                options: serde_json::to_string(&spec.options).unwrap_or_default(),
                state: ROLLOUT_DRAFT.into(),
                current_wave: 0,
                wave_started_at: None,
                soak_until: None,
                awaiting_continue: false,
                pause_reason: None,
                pause_cause: None,
                cancel_reason: None,
                created_at: now,
                created_by: who.actor.clone(),
                started_at: None,
                completed_at: None,
                updated_at: now,
                submitted_at: None,
                submitted_by: None,
                approved_at: None,
                approved_by: None,
                reject_reason: None,
            };
            s.insert_rollout(&r)?;
            who.event(
                "rollout",
                "rollout.create",
                Some(r.id.clone()),
                None,
                json!({"rollout_id": r.id, "name": r.name, "target_version": r.target_version, "selector": spec.selector, "waves": spec.waves_json(), "options": spec.options, "schedule": schedule.json(), "eligible": p.eligible}),
            )
            .append(s, now)?;
            let mut v = rollout_json(&r, &[], now, true);
            v["preview"] = p.json();
            Ok(v)
        })
        .await
        .map_err(|e| e.into_api("推送"))
    }

    /// `PUT /api/v1/rollouts/{id}`：只有草稿可以修改
    pub async fn update_rollout(
        &self,
        who: &Actor,
        id: String,
        spec: RolloutSpec,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let schedule = schedule_to_store(&spec, &self.default_timezone(), now)?;
        let dir = self.releases_dir.clone();
        let v = self.in_tx(move |s| {
            let mut r = s.get_rollout(&id)?.ok_or_else(|| TxError::Api(not_found(&id)))?;
            if r.state != ROLLOUT_DRAFT
                && r.state != ROLLOUT_SCHEDULED
                && r.state != ROLLOUT_PENDING_APPROVAL
            {
                return Err(invalid_state(&r, "修改（只有草稿、待核准與已排程而尚未開始的推送可以修改）").into());
            }
            let missing = submission::missing_files(s, &dir)?;
            let (release_id, ctx) = release_context(s, &spec.target_version, &missing)?;
            if let Some(min) = incompatible(&ctx) {
                return Err(incompatible_error(&spec.target_version, &min).into());
            }
            // 待核准或已排程的推送：退回草稿、刪除已固定的對象，修改後要重新送出（確認字串以新的對象計算；
            // 雙人覆核時核准也失效，需要重新送審）
            let reverted = r.state != ROLLOUT_DRAFT;
            if reverted {
                s.delete_rollout_hosts(&id)?;
                r.state = ROLLOUT_DRAFT.into();
                r.submitted_at = None;
                r.submitted_by = None;
                r.approved_at = None;
                r.approved_by = None;
                r.updated_at = now;
                s.update_rollout_state(&r)?;
            }
            s.update_rollout_schedule(&id, &schedule.json().to_string(), now)?;
            s.update_rollout_spec(
                &id,
                &spec.name,
                release_id,
                &serde_json::to_string(&spec.selector).unwrap_or_default(),
                &spec.waves_json().to_string(),
                &serde_json::to_string(&spec.options).unwrap_or_default(),
                now,
            )?;
            who.event(
                "rollout",
                "rollout.update",
                Some(id.clone()),
                None,
                json!({"rollout_id": id, "name": spec.name, "target_version": spec.target_version, "selector": spec.selector, "waves": spec.waves_json(), "options": spec.options, "schedule": schedule.json(), "reverted_to_draft": reverted}),
            )
            .append(s, now)?;
            let r = s.get_rollout(&id)?.ok_or_else(|| TxError::Api(not_found(&id)))?;
            Ok(rollout_json(&r, &[], now, true))
        })
        .await
        .map_err(|e| e.into_api("推送"))?;
        if let Some(id) = v["id"].as_str() {
            self.mark_dirty(id);
        }
        Ok(v)
    }

    /// `POST /api/v1/rollouts/{id}/submit`：固定推送的對象並立即開始（0.9 沒有覆核與排程）。
    /// L2：`confirm` 為「升級 N 台」，有無法自動回滾的主機（`allow_no_rollback`）時為「升級 N 台（無法自動回滾
    /// M 台）」（只接受這個字串）；降版 L3：「降版 N 台」且 `reason` 必填（0.9 不要求重新驗證密碼）
    pub async fn submit_rollout(
        &self,
        who: &Actor,
        id: String,
        confirm: Option<String>,
        reason: Option<String>,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let live = self.live_hellos();
        let approval = self.settings.approval_required();
        // 這次送出時生效的雙人覆核政策：寫進稽核（0.10.1），不依賴 meta 的前值
        let policy = self.settings.rollout_approval.clone();
        let dir = self.releases_dir.clone();
        let v = self
            .in_tx(move |s| {
                let mut r = s.get_rollout(&id)?.ok_or_else(|| TxError::Api(not_found(&id)))?;
                if r.state != ROLLOUT_DRAFT {
                    return Err(invalid_state(&r, "送出").into());
                }
                let schedule = ScheduleSpec::from_db(&r.schedule);
                if schedule.ends_at.is_some_and(|e| e <= now) {
                    return Err(ApiError::conflict("schedule_ended", "排程的結束時間（ends_at）已經過了，請先修改排程").into());
                }
                let (selector, waves, options) = spec_of(&r);
                // 版本庫的檔案在這裡重新檢查（ADR-0002 F1）：預覽之後檔案被刪除或大小改變時，需要它的主機
                // 以 artifact_missing 固定為不會升級，確認字串的台數也跟著改變
                let missing = submission::missing_files(s, &dir)?;
                let (_, ctx) = release_context(s, &r.target_version, &missing)?;
                if let Some(min) = incompatible(&ctx) {
                    return Err(incompatible_error(&r.target_version, &min).into());
                }
                let busy = rollout::busy_map(s.hosts_in_active_rollouts()?);
                let inventory = with_live(s.upgrade_hosts()?, &live);
                let (p, classified, assigned) = rollout::plan(&r.id, &inventory, &selector, &waves, &options, &ctx, &busy);
                if p.overlap_count > 0 {
                    return Err(overlap_error(&p).into());
                }
                if p.eligible == 0 {
                    return Err(ApiError::conflict("no_eligible_hosts", "沒有可以升級的主機（見預覽的資格分類）")
                        .with_details(p.json())
                        .into());
                }
                let downgrade = p.downgrade > 0;
                let expected = rollout::confirm_text(p.eligible, downgrade, p.no_rollback);
                if confirm.as_deref().map(str::trim) != Some(expected.as_str()) {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "confirmation_required",
                        format!("送出推送需要在 confirm 輸入「{expected}」（{}）", if downgrade { "L3" } else { "L2" }),
                    )
                    .with_details(json!({"confirm_text": expected, "confirm_level": if downgrade { "L3" } else { "L2" }, "eligible": p.eligible, "downgrade": p.downgrade, "no_rollback": p.no_rollback}))
                    .into());
                }
                if downgrade && reason.is_none() {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "confirmation_required",
                        "降版推送（L3）必須填寫 reason",
                    )
                    .into());
                }
                let wave_of: HashMap<i64, (i64, i64)> = assigned.iter().map(|(h, w, o)| (*h, (*w, *o))).collect();
                let mut rows = Vec::with_capacity(classified.len());
                for (h, e) in &classified {
                    let (wave, ordinal, state, reason) = match (e.skip, wave_of.get(&h.host_id)) {
                        (None, Some((w, o))) => (*w, *o, HOST_PENDING, None),
                        (Some(why), _) => (-1, 0, HOST_SKIPPED, Some(why.to_string())),
                        (None, None) => (-1, 0, HOST_SKIPPED, Some("not_assigned".to_string())),
                    };
                    rows.push(NewRolloutHost {
                        host_id: h.host_id,
                        upgrade_id: new_upgrade_id()?,
                        wave,
                        ordinal,
                        state: state.into(),
                        reason,
                        from_version: e.from_version.clone(),
                    });
                }
                s.insert_rollout_hosts(&r.id, &rows, now)?;
                r.submitted_at = Some(now);
                r.submitted_by = Some(who.actor.clone());
                r.approved_at = None;
                r.approved_by = None;
                r.reject_reason = None;
                // 雙人覆核：先 pending_approval，核准後才排程或開始。
                // 排程：starts_at 還沒到時先 scheduled（推送引擎到時轉為 running）
                let later = schedule.starts_at.is_some_and(|t| t > now);
                if approval {
                    r.state = ROLLOUT_PENDING_APPROVAL.into();
                } else if later {
                    r.state = ROLLOUT_SCHEDULED.into();
                } else {
                    r.state = ROLLOUT_RUNNING.into();
                    r.started_at = Some(now);
                    r.wave_started_at = Some(now);
                }
                r.updated_at = now;
                s.update_rollout_state(&r)?;
                let hosts = s.rollout_hosts(&r.id)?;
                // 送審內容的 digest（核准時在同一個交易重新計算並比對；稽核留下這次送出的是哪一份內容）
                let sub = submission::submission(&r, &hosts);
                who.event(
                    "rollout",
                    "rollout.submit",
                    Some(r.id.clone()),
                    None,
                    json!({"rollout_id": r.id, "name": r.name, "target_version": r.target_version, "eligible": p.eligible, "downgrade": p.downgrade, "no_rollback": p.no_rollback, "skipped": p.skipped, "waves": p.waves, "confirm_level": if downgrade { "L3" } else { "L2" }, "reason": reason, "schedule": schedule.json(), "state": r.state, "digest": sub.digest, "rollout_approval": policy}),
                )
                .append(s, now)?;
                if r.state == ROLLOUT_RUNNING {
                    who.event("rollout", "rollout.start", Some(r.id.clone()), None, start_details(&r, &policy, json!({})))
                        .append(s, now)?;
                    warn_unapproved_start(&r);
                }
                let mut v = rollout_json(&r, &hosts, now, true);
                if r.state == ROLLOUT_PENDING_APPROVAL {
                    let cx = submission::PendingContext {
                        inventory: inventory.into_iter().map(|h| (h.host_id, h)).collect(),
                        missing,
                        approvers: submission::approvers(s, now)?,
                    };
                    v["submission"] = submission::submission_json(s, &r, &hosts, &cx)?;
                }
                Ok(v)
            })
            .await
            .map_err(|e| e.into_api("推送"))?;
        if let Some(id) = v["id"].as_str() {
            self.mark_dirty(id);
        }
        self.wake();
        Ok(v)
    }

    /// `POST /api/v1/rollouts/{id}/approve`（雙人覆核，REMOTE.md 第 5.4 節；ADR-0002 決策點 2b）：
    /// `pending_approval` → `scheduled` 或 `running`。呼叫端（API）已確認是以帳號登入的工作階段、有
    /// `rollouts:approve`。**同一個交易**中依序檢查，全部通過才轉移狀態：
    ///
    /// 1. 狀態是 `pending_approval`（409 `invalid_state`）；
    /// 2. 核准者不是建立者或送出者（403 `self_approval`，`KRS-E6106`）；
    /// 3. 重新讀取已固定的對象、重新計算送審內容的 `digest`：請求的 `digest` 不符 → 409
    ///    `submission_changed`（核准者看到摘要之後推送被修改或重新送出）；
    /// 4. 重新計算確認字串「核准 N 台」（降版 L3「核准降版 N 台」）：`confirm`（去掉前後空白）不符 → 400
    ///    `confirmation_required`；沒有帶 `digest` → 400 `invalid_parameter`；降版而沒有 `reason` → 400；
    /// 5. 排程的結束時間還沒過（409 `schedule_ended`）；
    /// 6. 已固定的主機需要的發佈檔都還在版本庫（409 `artifact_missing`，`KRS-W8417`；ADR-0002 F2）
    pub async fn approve_rollout(
        &self,
        who: &Actor,
        id: String,
        req: ApproveRequest,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let live = self.live_hellos();
        let dir = self.releases_dir.clone();
        let policy = self.settings.rollout_approval.clone();
        let v = self
            .in_tx(move |s| {
                let mut r = s.get_rollout(&id)?.ok_or_else(|| TxError::Api(not_found(&id)))?;
                if r.state != ROLLOUT_PENDING_APPROVAL {
                    return Err(invalid_state(&r, "核准（只有待核准的推送可以核准）").into());
                }
                if who.actor == r.created_by || r.submitted_by.as_deref() == Some(who.actor.as_str()) {
                    return Err(ApiError::new(
                        StatusCode::FORBIDDEN,
                        "self_approval",
                        "不能核准自己建立或送出的推送，請由另一位有 rollouts:approve 權限的使用者核准",
                    )
                    .with_product_code(super::CODE_SELF_APPROVAL)
                    .with_details(json!({"created_by": r.created_by, "submitted_by": r.submitted_by}))
                    .into());
                }
                let hosts = s.rollout_hosts(&r.id)?;
                let sub = submission::submission(&r, &hosts);
                if req.digest.as_deref().is_some_and(|d| d.trim() != sub.digest) {
                    return Err(ApiError::conflict(
                        "submission_changed",
                        "這個推送在你檢視之後被修改或重新送出，請重新載入送審摘要後再核准",
                    )
                    .with_details(json!({"digest": sub.digest}))
                    .into());
                }
                let expected = sub.confirm_text();
                let level = sub.confirm_level();
                if req.confirm.as_deref().map(str::trim) != Some(expected.as_str()) {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "confirmation_required",
                        format!("核准推送需要在 confirm 輸入「{expected}」（{level}）"),
                    )
                    .with_details(json!({"confirm_text": expected, "confirm_level": level, "eligible": sub.eligible, "downgrade": sub.downgrade}))
                    .into());
                }
                if req.digest.is_none() {
                    return Err(ApiError::param(
                        "digest",
                        "核准推送需要 digest（推送的 submission.digest：你檢視的那一份送審內容）",
                    )
                    .into());
                }
                if sub.downgrade > 0 && req.reason.is_none() {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "confirmation_required",
                        "核准降版推送（L3）必須填寫 reason",
                    )
                    .with_details(json!({"confirm_text": expected, "confirm_level": level, "parameter": "reason"}))
                    .into());
                }
                let schedule = ScheduleSpec::from_db(&r.schedule);
                if schedule.ends_at.is_some_and(|e| e <= now) {
                    return Err(ApiError::conflict("schedule_ended", "排程的結束時間（ends_at）已經過了，請退回後修改排程").into());
                }
                // 已固定的主機需要的發佈檔：送出之後檔案被刪除或大小改變時不核准（狀態不變；補檔後再核准，
                // 或退回後重新送出）
                let missing = submission::missing_files(s, &dir)?;
                if !missing.is_empty() {
                    let inventory: HashMap<i64, super::store::UpgradeHost> =
                        with_live(s.upgrade_hosts()?, &live)
                            .into_iter()
                            .map(|h| (h.host_id, h))
                            .collect();
                    let problems = submission::fixed_problems(
                        &missing,
                        &r,
                        &rollout::options_from_db(&r.options),
                        &hosts,
                        &inventory,
                    );
                    if let Some(((version, filename, _), _)) = problems.iter().next() {
                        return Err(ApiError::conflict(
                            rollout::SKIP_ARTIFACT_MISSING,
                            format!("版本庫的檔案遺失或大小不符：Pulse {version} 的 {filename}。請在 Pulse 版本庫重新上傳後再核准，或退回這個推送"),
                        )
                        .with_product_code(CODE_RELEASE_FILE_MISSING)
                        .with_details(json!({"files": submission::problems_json(&problems)}))
                        .into());
                    }
                }
                r.approved_at = Some(now);
                r.approved_by = Some(who.actor.clone());
                if schedule.starts_at.is_some_and(|t| t > now) {
                    r.state = ROLLOUT_SCHEDULED.into();
                } else {
                    r.state = ROLLOUT_RUNNING.into();
                    r.started_at = Some(now);
                    r.wave_started_at = Some(now);
                }
                r.updated_at = now;
                s.update_rollout_state(&r)?;
                // 稽核留下核准的是哪一份內容（digest）、台數與原本的送審資訊
                who.event(
                    "rollout",
                    "rollout.approve",
                    Some(r.id.clone()),
                    None,
                    json!({"rollout_id": r.id, "name": r.name, "target_version": r.target_version, "created_by": r.created_by, "submitted_by": r.submitted_by, "submitted_at": r.submitted_at.map(crate::api::http::ts_ms), "state": r.state, "progress": rollout::progress(&hosts), "digest": sub.digest, "eligible": sub.eligible, "downgrade": sub.downgrade, "confirm_level": level, "reason": req.reason}),
                )
                .append(s, now)?;
                if r.state == ROLLOUT_RUNNING {
                    who.event("rollout", "rollout.start", Some(r.id.clone()), None, start_details(&r, &policy, json!({"approved": true})))
                        .append(s, now)?;
                }
                Ok(rollout_json(&r, &hosts, now, true))
            })
            .await
            .map_err(|e| e.into_api("推送"))?;
        if let Some(id) = v["id"].as_str() {
            self.mark_dirty(id);
        }
        self.wake();
        Ok(v)
    }

    /// `POST /api/v1/rollouts/{id}/reject`：退回待核准的推送（回到草稿、刪除已固定的對象，`reject_reason`
    /// 留給建立者看；修改後重新送出）
    pub async fn reject_rollout(
        &self,
        who: &Actor,
        id: String,
        reason: String,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let v = self
            .in_tx(move |s| {
                let mut r = s.get_rollout(&id)?.ok_or_else(|| TxError::Api(not_found(&id)))?;
                if r.state != ROLLOUT_PENDING_APPROVAL {
                    return Err(invalid_state(&r, "退回（只有待核准的推送可以退回）").into());
                }
                s.delete_rollout_hosts(&r.id)?;
                let (was_at, was_by) = (r.submitted_at.take(), r.submitted_by.take());
                r.state = ROLLOUT_DRAFT.into();
                r.approved_at = None;
                r.approved_by = None;
                r.reject_reason = Some(reason.clone());
                r.updated_at = now;
                s.update_rollout_state(&r)?;
                who.event(
                    "rollout",
                    "rollout.reject",
                    Some(r.id.clone()),
                    None,
                    json!({"rollout_id": r.id, "name": r.name, "reason": reason, "submitted_by": was_by, "submitted_at": was_at.map(crate::api::http::ts_ms)}),
                )
                .append(s, now)?;
                Ok(rollout_json(&r, &[], now, true))
            })
            .await
            .map_err(|e| e.into_api("推送"))?;
        if let Some(id) = v["id"].as_str() {
            self.mark_dirty(id);
        }
        Ok(v)
    }

    /// `POST /api/v1/rollouts/{id}/pause`
    pub async fn pause_rollout(
        &self,
        who: &Actor,
        id: String,
        reason: String,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let (v, cleared) = self
            .in_tx(move |s| {
                let mut r = s
                    .get_rollout(&id)?
                    .ok_or_else(|| TxError::Api(not_found(&id)))?;
                if r.state != ROLLOUT_RUNNING {
                    return Err(invalid_state(&r, "暫停").into());
                }
                let cleared = pause_in_tx(s, &mut r, &reason, &who, None, now)?;
                Ok((
                    rollout_json(&r, &s.rollout_hosts(&r.id)?, now, true),
                    cleared,
                ))
            })
            .await
            .map_err(|e| e.into_api("推送"))?;
        for a in cleared {
            self.set_offer(&a, None);
        }
        if let Some(id) = v["id"].as_str() {
            self.mark_dirty(id);
        }
        self.wake();
        Ok(v)
    }

    /// `POST /api/v1/rollouts/{id}/resume`
    pub async fn resume_rollout(&self, who: &Actor, id: String) -> Result<Value, ApiError> {
        self.change_state(who, id, "resume").await
    }

    /// `POST /api/v1/rollouts/{id}/continue`：觀察期滿、需要人工繼續的批次開始下一批
    pub async fn continue_rollout(&self, who: &Actor, id: String) -> Result<Value, ApiError> {
        self.change_state(who, id, "continue").await
    }

    async fn change_state(
        &self,
        who: &Actor,
        id: String,
        op: &'static str,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let v = self
            .in_tx(move |s| {
                let mut r = s.get_rollout(&id)?.ok_or_else(|| TxError::Api(not_found(&id)))?;
                match op {
                    "resume" => {
                        if r.state != ROLLOUT_PAUSED {
                            return Err(invalid_state(&r, "繼續（只有暫停中的推送可以繼續）").into());
                        }
                        let was = r.pause_reason.take();
                        r.pause_cause = None;
                        r.state = ROLLOUT_RUNNING.into();
                        r.updated_at = now;
                        s.update_rollout_state(&r)?;
                        who.event("rollout", "rollout.resume", Some(r.id.clone()), None, json!({"rollout_id": r.id, "pause_reason": was}))
                            .append(s, now)?;
                    }
                    _ => {
                        if r.state != ROLLOUT_RUNNING || !r.awaiting_continue {
                            return Err(invalid_state(&r, "開始下一批（目前沒有等待繼續的批次）").into());
                        }
                        let waves = rollout::waves_from_db(&r.waves).len();
                        let from = r.current_wave;
                        next_wave_in_tx(s, &mut r, waves, now)?;
                        who.event("rollout", "rollout.resume", Some(r.id.clone()), None, json!({"rollout_id": r.id, "continue": true, "from_wave": from, "to_wave": r.current_wave}))
                            .append(s, now)?;
                    }
                }
                Ok(rollout_json(&r, &s.rollout_hosts(&r.id)?, now, true))
            })
            .await
            .map_err(|e| e.into_api("推送"))?;
        if let Some(id) = v["id"].as_str() {
            self.mark_dirty(id);
        }
        self.wake();
        Ok(v)
    }

    /// `POST /api/v1/rollouts/{id}/cancel`：提議中尚未替換的主機收到 `upgrade_unavailable`，替換中的照常完成
    pub async fn cancel_rollout(
        &self,
        who: &Actor,
        id: String,
        reason: String,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let (v, cleared) = self
            .in_tx(move |s| {
                let mut r = s.get_rollout(&id)?.ok_or_else(|| TxError::Api(not_found(&id)))?;
                if r.state == ROLLOUT_COMPLETED || r.state == ROLLOUT_CANCELLED {
                    return Err(invalid_state(&r, "取消").into());
                }
                let hosts = s.rollout_hosts(&r.id)?;
                let mut cleared = Vec::new();
                for h in hosts.iter().filter(|h| rollout::is_offered(&h.state) || h.state == HOST_PENDING || h.state == HOST_SKIPPED_OFFLINE) {
                    s.update_rollout_host(
                        &r.id,
                        h.host_id,
                        &HostTransition {
                            state: Some(HOST_CANCELLED.into()),
                            finished_at: Some(Some(now)),
                            ..Default::default()
                        },
                        now,
                    )?;
                    if rollout::is_offered(&h.state) {
                        cleared.push(h.agent_id.clone());
                    }
                }
                r.state = ROLLOUT_CANCELLED.into();
                r.cancel_reason = Some(reason.clone());
                r.completed_at = Some(now);
                r.awaiting_continue = false;
                r.updated_at = now;
                s.update_rollout_state(&r)?;
                let hosts = s.rollout_hosts(&r.id)?;
                who.event("rollout", "rollout.cancel", Some(r.id.clone()), None, json!({"rollout_id": r.id, "name": r.name, "reason": reason, "progress": rollout::progress(&hosts)}))
                    .append(s, now)?;
                Ok((rollout_json(&r, &hosts, now, true), cleared))
            })
            .await
            .map_err(|e| e.into_api("推送"))?;
        for a in cleared {
            self.set_offer(&a, None);
        }
        if let Some(id) = v["id"].as_str() {
            self.mark_dirty(id);
        }
        self.wake();
        Ok(v)
    }

    /// `POST /api/v1/rollouts/{id}/hosts/{agent_id}/retry`：新的 upgrade_id 重新提議
    pub async fn retry_host(
        &self,
        who: &Actor,
        id: String,
        agent_id: String,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let rid = id.clone();
        let v = self
            .in_tx(move |s| {
                let r = s.get_rollout(&id)?.ok_or_else(|| TxError::Api(not_found(&id)))?;
                if r.state != ROLLOUT_RUNNING && r.state != ROLLOUT_PAUSED {
                    return Err(invalid_state(&r, "重試（推送已結束，請另建推送）").into());
                }
                let h = s.rollout_host(&id, &agent_id)?.ok_or_else(|| {
                    TxError::Api(ApiError::new(StatusCode::NOT_FOUND, "host_not_in_rollout", format!("{agent_id} 不在推送 {id} 中")))
                })?;
                if !rollout::is_retryable(&h) {
                    return Err(ApiError::conflict(
                        "invalid_state",
                        if h.state == HOST_ROLLBACK_FAILED {
                            "回滾失敗的主機需要先人工處理並標記為已處理（…/acknowledge）".to_string()
                        } else {
                            format!("{} 的狀態是 {}，不能重試", agent_id, h.state)
                        },
                    )
                    .with_details(json!({"state": h.state}))
                    .into());
                }
                s.update_rollout_host(
                    &id,
                    h.host_id,
                    &HostTransition {
                        state: Some(HOST_PENDING.into()),
                        upgrade_id: Some(new_upgrade_id()?),
                        bump_attempts: true,
                        reason: Some(None),
                        code: Some(None),
                        detail: Some(None),
                        offered_at: Some(None),
                        applying_at: Some(None),
                        finished_at: Some(None),
                        // 已處理的回滾失敗：重試後清除標記
                        acknowledged: None,
                        ..Default::default()
                    },
                    now,
                )?;
                // 重試的主機屬於已經過去的批次時，批次的觀察不受影響；推送已在觀察中也照常提議這一台
                who.event("rollout", "rollout.update", Some(id.clone()), None, json!({"rollout_id": id, "retry": agent_id, "previous_state": h.state, "attempts": h.attempts + 1}))
                    .append(s, now)?;
                let h = s.rollout_host(&id, &agent_id)?.unwrap_or(h);
                Ok(rollout::host_json(&h))
            })
            .await
            .map_err(|e| e.into_api("推送"))?;
        self.mark_dirty(&rid);
        self.wake();
        Ok(v)
    }

    /// `POST /api/v1/rollouts/{id}/hosts/{agent_id}/acknowledge`：回滾失敗已人工處理
    pub async fn acknowledge_host(
        &self,
        who: &Actor,
        id: String,
        agent_id: String,
        note: Option<String>,
    ) -> Result<Value, ApiError> {
        let now = self.now_ms();
        let who = who.clone();
        let rid = id.clone();
        let r = self
            .in_tx(move |s| {
                let h = s.rollout_host(&id, &agent_id)?.ok_or_else(|| {
                    TxError::Api(ApiError::new(
                        StatusCode::NOT_FOUND,
                        "host_not_in_rollout",
                        format!("{agent_id} 不在推送 {id} 中"),
                    ))
                })?;
                if h.state != HOST_ROLLBACK_FAILED {
                    return Err(ApiError::conflict(
                        "invalid_state",
                        "只有回滾失敗的主機需要標記為已處理",
                    )
                    .into());
                }
                s.update_rollout_host(
                    &id,
                    h.host_id,
                    &HostTransition {
                        acknowledged: Some((now, who.actor.clone())),
                        ..Default::default()
                    },
                    now,
                )?;
                who.event(
                    "host",
                    "host.upgrade.acknowledge",
                    Some(agent_id.clone()),
                    None,
                    json!({"rollout_id": id, "agent_id": agent_id, "note": note}),
                )
                .append(s, now)?;
                let h = s.rollout_host(&id, &agent_id)?.unwrap_or(h);
                Ok(rollout::host_json(&h))
            })
            .await
            .map_err(|e| e.into_api("推送"));
        if r.is_ok() {
            // 主控台的 `rollout.host` 與 `rollout.progress`
            self.mark_dirty(&rid);
            self.wake();
        }
        r
    }

    // ---- 推送引擎 ----

    /// 推送引擎的一輪：每個進行中的推送一個交易
    pub(crate) async fn tick(&self) -> Result<(), DbError> {
        let dir = self.releases_dir.clone();
        let (ids, missing): (Vec<String>, Vec<MissingFile>) = self
            .db
            .read(move |s| {
                // 進行中的推送，加上已排程（到 starts_at 時轉為進行中）的推送
                let mut ids: Vec<String> = s
                    .list_rollouts(Some(ROLLOUT_SCHEDULED))?
                    .into_iter()
                    .map(|r| r.id)
                    .collect();
                ids.extend(
                    s.list_rollouts(Some(ROLLOUT_RUNNING))?
                        .into_iter()
                        .map(|r| r.id),
                );
                // 版本庫的檔案（ADR-0002 F3）：有推送在進行時，每一輪檢查一次（每個已存入的檔案一次 stat）；
                // 檔案有問題的主機這一輪不提議
                let missing = if ids.is_empty() {
                    Vec::new()
                } else {
                    submission::missing_files(s, &dir)?
                };
                Ok((ids, missing))
            })
            .await?;
        let missing = Arc::new(missing);
        for id in ids {
            self.tick_one(id, missing.clone()).await?;
        }
        Ok(())
    }

    async fn tick_one(&self, id: String, missing: Arc<Vec<MissingFile>>) -> Result<(), DbError> {
        let rollout_id = id.clone();
        let now = self.now_ms();
        let started = self.started_ms;
        let live_capable = self.capable_agents();
        let reg2 = self.registry.clone();
        let policy = self.settings.rollout_approval.clone();
        // 有檔案遺失時才需要每台主機的安裝方式與平台（連線中的 hello）
        let live_hellos = if missing.is_empty() {
            HashMap::new()
        } else {
            self.live_hellos()
        };
        let r = self
            .in_tx(move |s| {
                let Some(mut r) = s.get_rollout(&id)? else {
                    return Ok((Vec::new(), Vec::new(), false));
                };
                let schedule = ScheduleSpec::from_db(&r.schedule);
                let mut started_now = false;
                if r.state == ROLLOUT_SCHEDULED {
                    // 已排程：到 starts_at 才開始（稽核的 rollout.start 此時才寫，執行者為 system）
                    if schedule.starts_at.is_some_and(|t| t > now) {
                        return Ok((Vec::new(), Vec::new(), false));
                    }
                    r.state = ROLLOUT_RUNNING.into();
                    r.started_at = Some(now);
                    r.wave_started_at = Some(now);
                    r.updated_at = now;
                    s.update_rollout_state(&r)?;
                    Actor::system()
                        .event("rollout", "rollout.start", Some(r.id.clone()), None, start_details(&r, &policy, json!({"scheduled": true})))
                        .append(s, now)?;
                    warn_unapproved_start(&r);
                    log::info!(event = "rollout.start"; "推送「{}」（{}）到達排程的開始時間，開始推送", r.name, r.id);
                    started_now = true;
                }
                if r.state != ROLLOUT_RUNNING {
                    return Ok((Vec::new(), Vec::new(), false));
                }
                let mut hosts = s.rollout_hosts(&r.id)?;
                let (_, waves, options) = spec_of(&r);
                // 版本庫的檔案遺失（ADR-0002 F3）：需要那個檔案的主機不提議、留在 pending（reason 為
                // artifact_missing）；已提議而還沒下載完的收回提議、回到 pending。不計入失敗、不暫停推送，
                // 補檔後下一輪清除 reason、照常提議
                let mut blocked: HashSet<String> = HashSet::new();
                let mut waiting_changed = false;
                let mut withdrawn = Vec::new();
                if !missing.is_empty() {
                    let inventory: HashMap<i64, super::store::UpgradeHost> =
                        with_live(s.upgrade_hosts()?, &live_hellos)
                            .into_iter()
                            .map(|h| (h.host_id, h))
                            .collect();
                    for h in hosts.iter().filter(|h| submission::waits_for_file(&h.state)) {
                        let u = inventory.get(&h.host_id);
                        let method = u.and_then(|u| u.install_method.clone()).unwrap_or_default();
                        let arch = u.and_then(|u| rollout::arch_of(u.os.as_deref())).unwrap_or_default();
                        if !submission::blocked_files(&missing, &r.target_version, &method, &arch, h.from_version.as_deref(), options.allow_no_rollback).is_empty() {
                            blocked.insert(h.agent_id.clone());
                        }
                    }
                }
                for h in &hosts {
                    let waiting = h.state == HOST_PENDING && h.reason.as_deref() == Some(rollout::SKIP_ARTIFACT_MISSING);
                    let t = if blocked.contains(&h.agent_id) {
                        if rollout::is_offered(&h.state) {
                            withdrawn.push(h.agent_id.clone());
                            log::warn!(event = "rollout.artifact_missing", code = CODE_RELEASE_FILE_MISSING; "警告 {CODE_RELEASE_FILE_MISSING}：推送「{}」（{}）：{} 需要的發佈檔不在版本庫（遺失或大小不符），收回提議，補檔後自動繼續", r.name, r.id, h.agent_id);
                            Some(HostTransition {
                                state: Some(HOST_PENDING.into()),
                                upgrade_id: Some(new_upgrade_id()?),
                                reason: Some(Some(rollout::SKIP_ARTIFACT_MISSING.into())),
                                offered_at: Some(None),
                                ..Default::default()
                            })
                        } else if !waiting {
                            Some(HostTransition {
                                reason: Some(Some(rollout::SKIP_ARTIFACT_MISSING.into())),
                                ..Default::default()
                            })
                        } else {
                            None
                        }
                    } else if waiting {
                        // 檔案補回來了
                        Some(HostTransition {
                            reason: Some(None),
                            ..Default::default()
                        })
                    } else {
                        None
                    };
                    if let Some(t) = t {
                        s.update_rollout_host(&r.id, h.host_id, &t, now)?;
                        waiting_changed = true;
                    }
                }
                if waiting_changed {
                    hosts = s.rollout_hosts(&r.id)?;
                }
                let started_now = started_now || waiting_changed;
                // ends_at 到了：尚未開始替換的主機標為 expired（rollout_ended），替換中的照常完成後推送才結束
                if schedule.ends_at.is_some_and(|e| now >= e) {
                    let mut cleared = withdrawn;
                    let mut changed = started_now;
                    for h in hosts.iter().filter(|h| {
                        rollout::is_offered(&h.state)
                            || h.state == HOST_PENDING
                            || h.state == HOST_SKIPPED_OFFLINE
                    }) {
                        s.update_rollout_host(
                            &r.id,
                            h.host_id,
                            &HostTransition {
                                state: Some(HOST_EXPIRED.into()),
                                reason: Some(Some(rollout::REASON_ROLLOUT_ENDED.into())),
                                finished_at: Some(Some(now)),
                                ..Default::default()
                            },
                            now,
                        )?;
                        if rollout::is_offered(&h.state) {
                            cleared.push(h.agent_id.clone());
                        }
                        changed = true;
                    }
                    if !hosts.iter().any(|h| h.state == HOST_APPLYING) {
                        log::info!(event = "rollout.ended"; "推送「{}」（{}）到達排程的結束時間", r.name, r.id);
                        complete_in_tx(s, &mut r, now)?;
                        changed = true;
                    }
                    return Ok((Vec::new(), cleared, changed));
                }
                let input = rollout::TickInput {
                    schedule: &schedule,
                    now_ms: now,
                    started_ms: started,
                    start_grace_ms: crate::testhooks::remote_start_grace_ms()
                        .unwrap_or(rollout::START_GRACE_MS),
                    current_wave: r.current_wave,
                    soak_until: r.soak_until,
                    awaiting_continue: r.awaiting_continue,
                    waves: &waves,
                    options: &options,
                };
                // 等待檔案的主機對引擎來說「不在」：不提議，也不因離線或沒有協商 upgrade 而跳過
                let live = |a: &str| {
                    if blocked.contains(a) {
                        rollout::HostLive::default()
                    } else {
                        live_of(&reg2, a, &live_capable)
                    }
                };
                let (actions, ra) = rollout::plan_tick(&input, &hosts, &live);
                let changed = started_now || !actions.is_empty() || ra.is_some();
                let mut set = Vec::new();
                let mut cleared = withdrawn;
                for (i, a) in actions {
                    let h = &hosts[i];
                    let t = match a {
                        HostAction::Offer => {
                            set.push((h.agent_id.clone(), h.upgrade_id.clone()));
                            HostTransition {
                                state: Some(HOST_OFFERED.into()),
                                offered_at: Some(Some(now)),
                                reason: Some(None),
                                ..Default::default()
                            }
                        }
                        HostAction::Expire => {
                            cleared.push(h.agent_id.clone());
                            HostTransition {
                                state: Some(HOST_EXPIRED.into()),
                                reason: Some(Some("not_started".into())),
                                finished_at: Some(Some(now)),
                                ..Default::default()
                            }
                        }
                        HostAction::Unknown => {
                            log::warn!(event = "rollout.host_unknown"; "推送「{}」：{} 替換後超過期限沒有回報且離線，標為 unknown（計入失敗）", r.name, h.agent_id);
                            HostTransition {
                                state: Some(HOST_UNKNOWN.into()),
                                reason: Some(Some("no_report_after_apply".into())),
                                finished_at: Some(Some(now)),
                                ..Default::default()
                            }
                        }
                        HostAction::SkipOffline => HostTransition {
                            state: Some(HOST_SKIPPED_OFFLINE.into()),
                            reason: Some(Some("offline".into())),
                            ..Default::default()
                        },
                        HostAction::SkipNotReady => HostTransition {
                            state: Some(HOST_SKIPPED.into()),
                            reason: Some(Some("not_ready".into())),
                            finished_at: Some(Some(now)),
                            ..Default::default()
                        },
                        HostAction::Revive => HostTransition {
                            state: Some(HOST_PENDING.into()),
                            reason: Some(None),
                            ..Default::default()
                        },
                        HostAction::Requeue => {
                            // 時段結束時還沒開始替換：下一個時段以新的 upgrade_id 重新提議
                            // （Pulse 對舊的 id 收到 upgrade_unavailable，已暫存的檔案可以沿用）
                            if rollout::is_offered(&h.state) {
                                cleared.push(h.agent_id.clone());
                            }
                            HostTransition {
                                state: Some(HOST_PENDING.into()),
                                upgrade_id: Some(new_upgrade_id()?),
                                reason: Some(Some("window_closed".into())),
                                bump_attempts: true,
                                offered_at: Some(None),
                                finished_at: Some(None),
                                code: Some(None),
                                detail: Some(None),
                                ..Default::default()
                            }
                        }
                    };
                    s.update_rollout_host(&r.id, h.host_id, &t, now)?;
                }
                match ra {
                    None => {}
                    Some(RolloutAction::Pause { failures, finished, ratio }) => {
                        let max = waves.get(r.current_wave as usize).map_or(0.0, |w| w.max_failure_ratio);
                        let reason = format!(
                            "第 {} 批的失敗比例 {:.0}%（{failures}／{finished}）超過上限 {:.0}%",
                            r.current_wave + 1,
                            ratio * 100.0,
                            max * 100.0
                        );
                        let details = json!({"cause": rollout::PAUSE_FAILURE_RATIO, "wave": r.current_wave, "failures": failures, "finished": finished, "ratio": ratio, "max_failure_ratio": max});
                        let mut c = pause_in_tx(s, &mut r, &reason, &Actor::system(), Some(details), now)?;
                        set.retain(|(a, _)| !c.contains(a));
                        cleared.append(&mut c);
                    }
                    Some(RolloutAction::StartSoak { until }) => {
                        r.soak_until = Some(until);
                        r.updated_at = now;
                        s.update_rollout_state(&r)?;
                    }
                    Some(RolloutAction::AwaitContinue) => {
                        r.awaiting_continue = true;
                        r.updated_at = now;
                        s.update_rollout_state(&r)?;
                    }
                    Some(RolloutAction::NextWave) => next_wave_in_tx(s, &mut r, waves.len(), now)?,
                    Some(RolloutAction::Complete) => complete_in_tx(s, &mut r, now)?,
                }
                Ok((set, cleared, changed))
            })
            .await;
        let (set, cleared, changed) = match r {
            Ok(v) => v,
            Err(TxError::Db(e)) => return Err(e),
            Err(TxError::Api(e)) => return Err(DbError(e.message)),
        };
        for a in cleared {
            self.set_offer(&a, None);
        }
        for (a, u) in set {
            log::debug!("升級推送：提議 {a}（upgrade_id {u}）");
            self.set_offer(&a, Some(&u));
        }
        if changed {
            self.mark_dirty(&rollout_id);
        }
        Ok(())
    }

    // ---- 8443 ----

    /// 組出 `upgrade_offer`（`upgrade_get`）；這台主機目前沒有這個提議時 `None`（`upgrade_unavailable`）
    pub async fn build_offer(
        &self,
        agent_id: &str,
        upgrade_id: &str,
        seq: u64,
    ) -> Result<Option<UpgradeOffer>, DbError> {
        if self.offer_of(agent_id).as_deref() != Some(upgrade_id) {
            return Ok(None);
        }
        let Some(session) = self.session(agent_id) else {
            return Ok(None);
        };
        let (agent, upgrade) = (agent_id.to_string(), upgrade_id.to_string());
        let now = self.now_ms();
        let dir = self.releases_dir.clone();
        let built = self
            .db
            .read(move |s| {
                let Some(h) = s.rollout_host_by_upgrade_id(&upgrade)? else {
                    return Ok(None);
                };
                if h.agent_id != agent || !rollout::is_offered(&h.state) {
                    return Ok(None);
                }
                let Some(r) = s.get_rollout(&h.rollout_id)? else {
                    return Ok(None);
                };
                if r.state != ROLLOUT_RUNNING {
                    return Ok(None);
                }
                let Some(rel) = s.get_release_by_id(r.release_id)? else {
                    return Ok(None);
                };
                let options = rollout::options_from_db(&r.options);
                let method = session.install_method.clone().unwrap_or_default();
                let arch = session.arch.clone().unwrap_or_default();
                let external = rel.source == super::ops::SOURCE_EXTERNAL;
                let artifacts = super::ops::offerable(&rel, s.release_artifacts(rel.id)?);
                let Some(a) = rollout::pick_artifact(&artifacts, &method, &arch).cloned() else {
                    return Ok(None);
                };
                // 版本庫的檔案遺失或大小不符（ADR-0002 F3）：不發出無法下載的提議。推送引擎的下一輪把這台
                // 主機收回到 pending（reason artifact_missing），補檔後自動繼續
                let file_problem = |a: &super::store::ArtifactRow| {
                    a.stored && super::releases::file_problem(&dir, &a.sha256, a.size).is_some()
                };
                if !external && file_problem(&a) {
                    return Ok(None);
                }
                let current = Version::parse(&session.agent_version).ok();
                let target = Version::parse(&rel.version).ok();
                let downgrade = matches!((&current, &target), (Some(c), Some(t)) if t < c);
                // 回滾檔：deb／rpm、主機目前的版本、同格式同平台
                // 回滾檔所屬的版本若是外部 URL 的版本庫，只在目標也是外部 URL 時附上（見 release_context）
                let rollback = if matches!(method.as_str(), "deb" | "rpm") {
                    match s.get_release(&session.agent_version)? {
                        Some(cur) if cur.source != super::ops::SOURCE_EXTERNAL || external => {
                            let list = super::ops::offerable(&cur, s.release_artifacts(cur.id)?);
                            rollout::pick_artifact(&list, &method, &arch)
                                .cloned()
                                .map(|a| (a, cur.base_url.clone()))
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                // 回滾檔在代管的版本庫中遺失：允許無法自動回滾的推送不附回滾檔，否則不提議
                let rollback = match rollback {
                    Some((b, None)) if file_problem(&b) => {
                        if !options.allow_no_rollback {
                            return Ok(None);
                        }
                        None
                    }
                    other => other,
                };
                // 外部 URL：`artifact.url` 為 base_url ＋檔名，Pulse 依本機白名單下載；簽章、SHA256SUMS 與
                // 雜湊照舊附在提議中，Pulse 的驗證與從 Kairos 下載完全相同
                let artifact_ref =
                    |a: &super::store::ArtifactRow, base: Option<&str>| ArtifactRef {
                        filename: a.filename.clone(),
                        size: a.size.max(0) as u64,
                        sha256: a.sha256.clone(),
                        source: if base.is_some() {
                            mgmt::SOURCE_URL
                        } else {
                            SOURCE_KAIROS
                        }
                        .to_string(),
                        url: base.map(|b| format!("{b}{}", a.filename)),
                    };
                let offered = h.offered_at.unwrap_or(r.updated_at);
                // apply_window：沒有升級時段時為提議後 24 小時；有時段時到提議所在的時段結束為止
                // （都不超過 ends_at）。時段已過（Pulse 交付前的再確認）時不再提議：upgrade_unavailable，
                // Pulse 不會在時段外替換，主機在下一個時段以新的 upgrade_id 重新提議
                let schedule = ScheduleSpec::from_db(&r.schedule);
                let Some((not_before, not_after)) = schedule
                    .apply_window(offered, rollout::APPLY_WINDOW_MS)
                    .filter(|(_, end)| now < *end)
                else {
                    return Ok(None);
                };
                let offer = UpgradeOffer {
                    seq,
                    upgrade_id: h.upgrade_id.clone(),
                    rollout_id: r.id.clone(),
                    action: if downgrade {
                        ACTION_DOWNGRADE
                    } else {
                        ACTION_UPGRADE
                    }
                    .to_string(),
                    target: UpgradeTarget {
                        version: rel.version.clone(),
                        commit: rel.commit.clone(),
                    },
                    artifact: artifact_ref(&a, rel.base_url.as_deref()),
                    rollback_artifact: rollback
                        .as_ref()
                        .map(|(b, base)| artifact_ref(b, base.as_deref())),
                    manifest: match (&rel.manifest_name, &rel.manifest) {
                        (Some(n), Some(c)) => Some(ManifestRef {
                            filename: n.clone(),
                            content: c.clone(),
                        }),
                        _ => None,
                    },
                    sha256sums: rel.sha256sums.clone(),
                    signature: rel.signature.clone(),
                    apply_window: ApplyWindow {
                        not_before: crate::api::http::ts_ms(not_before),
                        not_after: crate::api::http::ts_ms(not_after),
                    },
                    health_timeout_secs: options.health_timeout_secs,
                    allow_downgrade: options.allow_downgrade,
                };
                // 經 8443 提供的檔案（外部 URL 的檔案不經這條連線）
                let mut files = Vec::new();
                if rel.base_url.is_none() {
                    files.push((a.filename.clone(), a.sha256.clone(), a.size.max(0) as u64));
                }
                if let Some((b, None)) = &rollback {
                    files.push((b.filename.clone(), b.sha256.clone(), b.size.max(0) as u64));
                }
                Ok(Some((offer, files)))
            })
            .await?;
        let Some((offer, files)) = built else {
            return Ok(None);
        };
        if let Err(e) = mgmt::validate_upgrade_offer(&offer) {
            log::error!("升級推送：組出的 upgrade_offer 不合規定（{agent_id}，{upgrade_id}）：{e}");
            return Ok(None);
        }
        self.cache_files(
            upgrade_id,
            Arc::new(OfferFiles {
                agent_id: agent_id.to_string(),
                files,
            }),
        );
        Ok(Some(offer))
    }

    /// `upgrade_chunk_get` 可以讀的檔案：(路徑, 大小)；不是這次提議的檔案時 `None`（`upgrade_unavailable`）
    pub async fn offered_file(
        &self,
        agent_id: &str,
        upgrade_id: &str,
        filename: &str,
    ) -> Result<Option<(std::path::PathBuf, u64)>, DbError> {
        if self.offer_of(agent_id).as_deref() != Some(upgrade_id) {
            return Ok(None);
        }
        let files = match self.cached_files(upgrade_id) {
            Some(f) => f,
            None => {
                // Kairos 重新啟動後的續傳：重新組出提議
                if self.build_offer(agent_id, upgrade_id, 0).await?.is_none() {
                    return Ok(None);
                }
                match self.cached_files(upgrade_id) {
                    Some(f) => f,
                    None => return Ok(None),
                }
            }
        };
        if files.agent_id != agent_id {
            return Ok(None);
        }
        Ok(files
            .files
            .iter()
            .find(|(n, _, _)| n == filename)
            .map(|(_, sha, size)| {
                (
                    super::releases::artifact_path(&self.releases_dir, sha),
                    *size,
                )
            }))
    }

    /// `upgrade_status`：更新推送中這台主機的狀態（以 (`upgrade_id`, `state`) 去重；不認得的 upgrade_id
    /// 也回 `ack`）。`agent_version` 為這條連線 hello 的版本（`succeeded` 的交叉確認）
    pub async fn apply_upgrade_status(
        &self,
        agent_id: &str,
        agent_version: &str,
        st: UpgradeStatus,
    ) -> Result<(), DbError> {
        let now = self.now_ms();
        let agent = agent_id.to_string();
        let version = agent_version.to_string();
        let r = self
            .in_tx(move |s| {
                let Some(h) = s.rollout_host_by_upgrade_id(&st.upgrade_id)? else {
                    log::debug!("升級推送：{agent} 回報不認得的 upgrade_id {}（{}）", st.upgrade_id, st.state);
                    return Ok(None);
                };
                if h.agent_id != agent {
                    log::warn!("升級推送：{agent} 回報了屬於 {} 的 upgrade_id {}，忽略", h.agent_id, st.upgrade_id);
                    return Ok(None);
                }
                let Some(mut r) = s.get_rollout(&h.rollout_id)? else {
                    return Ok(None);
                };
                let progress = st.progress;
                if h.state == st.state {
                    // 重送或進度：只更新進度
                    if let Some(p) = progress {
                        s.update_rollout_host(&r.id, h.host_id, &HostTransition { bytes_done: Some(p.bytes as i64), bytes_total: Some(p.total as i64), ..Default::default() }, now)?;
                    }
                    return Ok(None);
                }
                // 取消時 Kairos 以為還沒開始替換、Pulse 其實已交給輔助程式：照實記錄替換與結果
                let after_cancel = h.state == HOST_CANCELLED
                    && matches!(st.state.as_str(), HOST_APPLYING | HOST_SUCCEEDED | HOST_FAILED | HOST_ROLLED_BACK | HOST_ROLLBACK_FAILED);
                if (rollout::is_terminal(&h.state) || h.state == HOST_PENDING) && !after_cancel {
                    // 已結束（或暫停後回到 pending）：晚到的回報只記錄
                    log::debug!("升級推送：{agent} 的 {} 已是 {}，忽略晚到的 {}", st.upgrade_id, h.state, st.state);
                    return Ok(None);
                }
                if !after_cancel && rank(&st.state) < rank(&h.state) {
                    log::debug!("升級推送：{agent} 的 {} 從 {} 回報 {}（較早的狀態），忽略", st.upgrade_id, h.state, st.state);
                    return Ok(None);
                }
                let mut t = HostTransition {
                    state: Some(st.state.clone()),
                    code: Some(st.code.clone()),
                    detail: Some((!st.detail.is_empty()).then(|| st.detail.clone())),
                    from_version: (!st.from_version.is_empty()).then(|| st.from_version.clone()),
                    to_version: (!st.to_version.is_empty()).then(|| st.to_version.clone()),
                    bytes_done: progress.map(|p| p.bytes as i64),
                    bytes_total: progress.map(|p| p.total as i64),
                    ..Default::default()
                };
                let mut events: Vec<(&'static str, Option<&'static str>)> = Vec::new();
                let mut pause: Option<(String, Value, bool)> = None;
                match st.state.as_str() {
                    HOST_ACCEPTED | HOST_STAGED => {}
                    HOST_APPLYING => t.applying_at = Some(Some(now)),
                    HOST_DECLINED => {
                        t.reason = Some(Some(if st.detail.is_empty() { st.code.clone().unwrap_or_else(|| "declined".into()) } else { st.detail.clone() }));
                        t.finished_at = Some(Some(now));
                    }
                    HOST_SUCCEEDED => {
                        t.finished_at = Some(Some(now));
                        let same = Version::parse(&version).ok().zip(Version::parse(&r.target_version).ok()).is_some_and(|(a, b)| a == b);
                        if !same {
                            log::warn!(event = "rollout.version_mismatch", code = CODE_VERSION_MISMATCH; "警告 {CODE_VERSION_MISMATCH}：{agent} 回報升級成功，但目前的版本是 {version}（目標 {}），標為失敗", r.target_version);
                            t.state = Some(HOST_FAILED.into());
                            t.reason = Some(Some("version_mismatch".into()));
                            t.code = Some(Some(CODE_VERSION_MISMATCH.into()));
                        }
                    }
                    HOST_FAILED => {
                        t.finished_at = Some(Some(now));
                        if st.code.as_deref() == Some("PLS-E8401") {
                            events.push(("host.upgrade.signature_failed", Some(CODE_SIGNATURE_FAILED)));
                            log::error!(event = "rollout.signature_failed", code = CODE_SIGNATURE_FAILED; "錯誤 {CODE_SIGNATURE_FAILED}：{agent} 驗證 Pulse {} 發佈包的簽章失敗（版本庫或傳輸可能被竄改，或信任錨與發佈不符）：{}", r.target_version, st.detail);
                            pause = Some((format!("{agent} 回報發佈包的簽章驗證失敗（PLS-E8401）"), json!({"cause": rollout::PAUSE_SIGNATURE_FAILED, "agent_id": agent, "detail": st.detail}), true));
                        }
                    }
                    HOST_ROLLED_BACK => {
                        t.finished_at = Some(Some(now));
                        events.push(("host.upgrade.rolled_back", Some(CODE_ROLLED_BACK)));
                        log::warn!(event = "rollout.rolled_back", code = CODE_ROLLED_BACK; "警告 {CODE_ROLLED_BACK}：{agent} 升級到 {} 後沒有恢復正常，已自動回滾到 {}", r.target_version, st.from_version);
                    }
                    HOST_ROLLBACK_FAILED => {
                        t.finished_at = Some(Some(now));
                        events.push(("host.upgrade.rollback_failed", Some(CODE_ROLLBACK_FAILED)));
                        log::error!(event = "rollout.rollback_failed", code = CODE_ROLLBACK_FAILED; "錯誤 {CODE_ROLLBACK_FAILED}：{agent} 升級回滾失敗，需要人工處理：{}", st.detail);
                        pause = Some((format!("{agent} 回滾失敗，需要人工處理"), json!({"cause": rollout::PAUSE_ROLLBACK_FAILED, "agent_id": agent}), false));
                    }
                    HOST_EXPIRED => t.finished_at = Some(Some(now)),
                    _ => {}
                }
                s.update_rollout_host(&r.id, h.host_id, &t, now)?;
                match t.state.as_deref() {
                    Some(HOST_SUCCEEDED) => log::info!(
                        event = "rollout.host_succeeded", code = super::CODE_UPGRADED;
                        "資訊 {}：{agent} 已升級到 Pulse {}（推送「{}」）",
                        super::CODE_UPGRADED,
                        r.target_version,
                        r.name
                    ),
                    Some(HOST_FAILED | HOST_DECLINED) => {
                        let code = st.code.as_deref().and_then(super::mirror_code);
                        log::info!(
                            event = "rollout.host_finished";
                            "升級推送「{}」：{agent} {}{}{}",
                            r.name,
                            t.state.as_deref().unwrap_or_default(),
                            code.map(|c| format!("（{c}）")).unwrap_or_default(),
                            if st.detail.is_empty() { String::new() } else { format!("：{}", st.detail) }
                        );
                    }
                    _ => {}
                }
                for (action, code) in events {
                    Actor::system()
                        .event("host", action, Some(agent.clone()), code, json!({"rollout_id": r.id, "upgrade_id": st.upgrade_id, "target_version": r.target_version, "from_version": st.from_version, "to_version": st.to_version, "code": st.code, "detail": st.detail}))
                        .append(s, now)?;
                }
                let mut cleared = Vec::new();
                if let Some((reason, details, all_of_release)) = pause {
                    if r.state == ROLLOUT_RUNNING {
                        cleared.extend(pause_in_tx(s, &mut r, &reason, &Actor::system(), Some(details.clone()), now)?);
                    }
                    if all_of_release {
                        // 簽章失敗：同一版本的其他推送也暫停
                        for mut other in s.list_rollouts(Some(ROLLOUT_RUNNING))? {
                            if other.release_id == r.release_id && other.id != r.id {
                                cleared.extend(pause_in_tx(s, &mut other, &reason, &Actor::system(), Some(details.clone()), now)?);
                            }
                        }
                    }
                }
                let terminal = t.state.as_deref().is_some_and(|x| rollout::is_terminal(x) || x == HOST_APPLYING);
                // 離線告警抑制（第 4.7.5 節）：進入 applying 時開始，到達終止狀態時結束
                let window = match t.state.as_deref() {
                    Some(HOST_APPLYING) => Some(Some((
                        r.id.clone(),
                        rollout::suppress_until(now, rollout::options_from_db(&r.options).health_timeout_secs),
                    ))),
                    Some(x) if rollout::is_terminal(x) => Some(None),
                    _ => None,
                };
                Ok(Some((terminal, cleared, r.id.clone(), window)))
            })
            .await;
        match r {
            Ok(Some((terminal, cleared, rollout_id, window))) => {
                if let Some(w) = window {
                    self.set_upgrade_window(agent_id, w);
                }
                self.mark_dirty(&rollout_id);
                if terminal {
                    // 已交給輔助程式或已結束：不再提議、釋出下載名額
                    self.set_offer(agent_id, None);
                }
                for a in cleared {
                    self.set_offer(&a, None);
                }
                self.wake();
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(TxError::Db(e)) => Err(e),
            Err(TxError::Api(e)) => Err(DbError(e.message)),
        }
    }

    /// `staged`：下載完成，釋出下載名額
    pub fn on_staged(&self, agent_id: &str) {
        self.release_download(agent_id);
    }
}

/// 連線中主機最近一次 hello 的資訊（推送的資格以它為準：清冊每 30 秒才同步一次 `install`、版本與標籤）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveHello {
    pub agent_version: String,
    pub install_method: Option<String>,
    pub install_upgrade: Option<String>,
    pub install_config: Option<String>,
    /// hello 的 `os`（JSON；資格分類只用 `arch`）
    pub os: Option<String>,
    pub tags: Option<Vec<String>>,
}

impl LiveHello {
    fn of(l: &crate::registry::LiveAgent) -> LiveHello {
        let host = l.host.as_deref();
        let install = host.and_then(|h| h.install.as_ref());
        LiveHello {
            agent_version: l.agent_version.to_string(),
            install_method: install.and_then(|i| i.method.clone()),
            install_upgrade: install.and_then(|i| i.upgrade.clone()),
            install_config: install.and_then(|i| i.config.clone()),
            os: host
                .and_then(|h| h.os.as_ref())
                .and_then(|o| serde_json::to_string(o).ok()),
            tags: host.and_then(|h| h.tags.clone()),
        }
    }

    /// 協商了遠端管理的連線（發提議時使用的資訊）優先
    fn overlay(&mut self, s: &super::AgentSession) {
        self.agent_version = s.agent_version.clone();
        if s.install_method.is_some() {
            self.install_method = s.install_method.clone();
        }
        if s.install_upgrade.is_some() {
            self.install_upgrade = s.install_upgrade.clone();
        }
        if let Some(arch) = &s.arch {
            let mut os: serde_json::Map<String, Value> = self
                .os
                .as_deref()
                .and_then(|t| serde_json::from_str(t).ok())
                .unwrap_or_default();
            os.insert("arch".into(), json!(arch));
            self.os = Some(Value::Object(os).to_string());
        }
    }
}

impl RemoteService {
    /// 連線中的主機（狀態表）的 hello，再以協商了遠端管理的連線（[`super::AgentSession`]，發提議時使用）覆蓋：
    /// 預覽、建立與送出都以這份為準，與發提議時一致，主機連線後不必等清冊同步
    pub(crate) fn live_hellos(&self) -> HashMap<String, LiveHello> {
        let mut out: HashMap<String, LiveHello> = self
            .registry
            .live_view()
            .iter()
            .filter(|l| l.online)
            .map(|l| (l.agent_id.to_string(), LiveHello::of(l)))
            .collect();
        for (a, s) in self.sessions_snapshot() {
            out.entry(a).or_default().overlay(&s);
        }
        out
    }

    /// 一台主機的 [`RemoteService::live_hellos`]（離線時 `None`）
    pub fn live_hello(&self, agent_id: &str) -> Option<LiveHello> {
        let mut h = self
            .registry
            .live_get(agent_id)
            .filter(|l| l.online)
            .map(|l| LiveHello::of(&l));
        if let Some(s) = self.session(agent_id) {
            h.get_or_insert_with(LiveHello::default).overlay(&s);
        }
        h
    }
}

/// 清冊中的主機以連線中的 hello（[`RemoteService::live_hellos`]）覆蓋；hello 沒有帶的欄位沿用清冊。
/// 清冊還沒有的主機（連線後尚未寫入過任何資料）不在推送的對象中（沒有 `host_id`）
pub(crate) fn with_live(
    mut hosts: Vec<super::store::UpgradeHost>,
    live: &HashMap<String, LiveHello>,
) -> Vec<super::store::UpgradeHost> {
    for h in hosts.iter_mut() {
        if let Some(l) = live.get(&h.agent_id) {
            h.agent_version = Some(l.agent_version.clone());
            if l.install_method.is_some() {
                h.install_method = l.install_method.clone();
            }
            if l.install_upgrade.is_some() {
                h.install_upgrade = l.install_upgrade.clone();
            }
            if l.install_config.is_some() {
                h.install_config = l.install_config.clone();
            }
            if l.os.is_some() {
                h.os = l.os.clone();
            }
            if let Some(tags) = &l.tags {
                h.tags = tags.clone();
            }
            h.online = true;
        }
    }
    hosts
}

/// 狀態的先後（晚到的較早狀態不覆蓋）
fn rank(state: &str) -> u8 {
    match state {
        HOST_PENDING => 0,
        HOST_OFFERED => 1,
        HOST_ACCEPTED => 2,
        HOST_STAGED => 3,
        HOST_APPLYING => 4,
        _ => 5,
    }
}

/// 一台主機的即時狀態（推送引擎）
fn live_of(
    registry: &crate::registry::Registry,
    agent_id: &str,
    capable: &HashSet<String>,
) -> rollout::HostLive {
    let live = registry.live_get(agent_id);
    let online = live.as_ref().is_some_and(|l| l.online);
    rollout::HostLive {
        capable: online && capable.contains(agent_id),
        online,
        offline_for_ms: live
            .as_ref()
            .and_then(|l| l.offline_for)
            .map(|d| d.as_millis() as i64),
        offline_by_server: live
            .as_ref()
            .and_then(|l| l.offline_reason)
            .is_some_and(|r| {
                matches!(
                    r,
                    crate::registry::OfflineReason::ServerShutdown
                        | crate::registry::OfflineReason::ServerLost
                )
            }),
    }
}

fn incompatible_error(version: &str, min: &str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "incompatible_release",
        format!(
            "Pulse {version} 需要 Kairos {min} 以上（目前 {}）",
            crate::VERSION
        ),
    )
    .with_product_code(CODE_INCOMPATIBLE)
    .with_details(json!({"min_kairos": min, "kairos_version": crate::VERSION}))
}

fn overlap_error(p: &rollout::Preview) -> ApiError {
    ApiError::conflict(
        "rollout_overlap",
        format!("{} 台主機已在另一個進行中的推送（一台主機同時只能屬於一個推送）", p.overlap_count),
    )
    .with_details(json!({
        "count": p.overlap_count,
        "agent_ids": p.overlap.iter().map(|(a, _)| a.clone()).collect::<Vec<_>>(),
        "hosts": p.overlap.iter().map(|(a, r)| json!({"agent_id": a, "rollout_id": r})).collect::<Vec<_>>(),
    }))
}

impl From<TxError> for DbError {
    fn from(e: TxError) -> Self {
        match e {
            TxError::Db(e) => e,
            TxError::Api(a) => DbError(a.message),
        }
    }
}
