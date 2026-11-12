//! 雙人覆核的送審摘要與版本庫檔案的檢查（ADR-0002 切片 S1、S2；REMOTE.md 第 5.4 節）。
//!
//! - [`submission`]：送出（`submit`）時固定下來的內容的摘要——`digest`（SHA-256）、確認字串「核准 N 台」、
//!   各分類與每批的台數。**只由 `rollouts` 與 `rollout_hosts` 的內容計算**（純函式），核准的交易以同一個函式
//!   重新計算並比對請求（[`super::RemoteService::approve_rollout`]）；主控台顯示的摘要與核准時驗證的是同一份。
//! - [`submission_json`]：主控台用的 `submission` 物件，另加依目前狀態計算的 `no_rollback`、`warnings[]`、
//!   `artifact_problems[]`、`approvers_available`。
//! - [`approvers`]、[`Viewer`]：誰可以核准（由後端計算，主控台不以帳號字串比對）。
//! - [`missing_files`]、[`blocked_files`]：版本庫中資料庫記為已存入、但檔案不存在或大小不符的發佈檔
//!   （`KRS-W8417`；只 `stat`，與系統健康相同），預覽、送出、核准、提議四處使用。

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use kairos_proto::release::Version;

use super::ops::{SOURCE_EXTERNAL, TxError};
use super::rollout::{self, HOST_SKIPPED, ReleaseContext, RolloutOptions};
use super::store::{ArtifactRow, RolloutHostRow, RolloutRow, UpgradeHost};
use crate::rbac::Permission;
use crate::store::db::{Store, StoreResult};

// ---- 版本庫的檔案 ----

/// 已存入（`stored`）但檔案有問題的發佈檔（代管的版本；外部 URL 的版本沒有本機檔案）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFile {
    pub version: String,
    pub artifact: ArtifactRow,
    /// `missing` 或 `size_mismatch`（[`super::releases::file_problem`]）
    pub problem: &'static str,
}

/// 版本庫中所有檔案有問題的發佈檔（每個已存入的檔案一次 `stat`；呼叫端在 blocking 執行緒）
pub fn missing_files(s: &dyn Store, dir: &Path) -> StoreResult<Vec<MissingFile>> {
    let releases: HashMap<i64, super::store::ReleaseRow> =
        s.list_releases()?.into_iter().map(|r| (r.id, r)).collect();
    let mut out = Vec::new();
    for a in s.all_artifacts()? {
        if !a.stored {
            continue;
        }
        let Some(r) = releases.get(&a.release_id) else {
            continue;
        };
        if r.source == SOURCE_EXTERNAL {
            continue;
        }
        if let Some(problem) = super::releases::file_problem(dir, &a.sha256, a.size) {
            out.push(MissingFile {
                version: r.version.clone(),
                artifact: a,
                problem,
            });
        }
    }
    Ok(out)
}

/// 這台主機需要、而檔案有問題的發佈檔：目標版本的檔案；deb／rpm 且推送不允許無法自動回滾的主機時，另含
/// 回滾檔（主機原本的版本）。空的表示可以提議
pub fn blocked_files<'a>(
    missing: &'a [MissingFile],
    target_version: &str,
    method: &str,
    arch: &str,
    from_version: Option<&str>,
    allow_no_rollback: bool,
) -> Vec<&'a MissingFile> {
    let pick = |version: &str| {
        missing.iter().find(|m| {
            m.version == version
                && rollout::pick_artifact(std::slice::from_ref(&m.artifact), method, arch).is_some()
        })
    };
    let mut out = Vec::new();
    if let Some(m) = pick(target_version) {
        out.push(m);
    }
    if !allow_no_rollback
        && matches!(method, "deb" | "rpm")
        && let Some(m) = from_version.and_then(pick)
    {
        out.push(m);
    }
    out
}

/// 清冊（已套用連線中的 hello）中一台主機的安裝方式與平台
fn method_arch(h: Option<&UpgradeHost>) -> (String, String) {
    (
        h.and_then(|h| h.install_method.clone())
            .unwrap_or_else(|| "unknown".to_string()),
        h.and_then(|h| rollout::arch_of(h.os.as_deref()))
            .unwrap_or_default(),
    )
}

/// 已固定的對象中，尚未開始替換的主機所需要而有問題的檔案：(版本, 檔名, 問題) → 受影響的主機數
pub fn fixed_problems(
    missing: &[MissingFile],
    r: &RolloutRow,
    options: &RolloutOptions,
    hosts: &[RolloutHostRow],
    inventory: &HashMap<i64, UpgradeHost>,
) -> BTreeMap<(String, String, &'static str), usize> {
    let mut out = BTreeMap::new();
    if missing.is_empty() {
        return out;
    }
    for h in hosts.iter().filter(|h| waits_for_file(&h.state)) {
        let (method, arch) = method_arch(inventory.get(&h.host_id));
        for m in blocked_files(
            missing,
            &r.target_version,
            &method,
            &arch,
            h.from_version.as_deref(),
            options.allow_no_rollback,
        ) {
            *out.entry((m.version.clone(), m.artifact.filename.clone(), m.problem))
                .or_default() += 1;
        }
    }
    out
}

/// 還需要下載檔案的主機狀態（`staged` 之後檔案已在主機上）
pub fn waits_for_file(state: &str) -> bool {
    matches!(
        state,
        rollout::HOST_PENDING | rollout::HOST_OFFERED | rollout::HOST_ACCEPTED
    )
}

/// [`fixed_problems`] 的 JSON（`submission.artifact_problems`、409 `artifact_missing` 的 `details.files`）
pub fn problems_json(p: &BTreeMap<(String, String, &'static str), usize>) -> Value {
    Value::Array(
        p.iter()
            .map(|((version, filename, problem), hosts)| {
                json!({"version": version, "filename": filename, "file_problem": problem, "hosts": hosts})
            })
            .collect(),
    )
}

// ---- 送審摘要 ----

/// 送審內容的摘要（只由資料庫中已固定的內容計算）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    /// 送審內容的 SHA-256（十六進位）：推送的內容＋已固定的主機清單＋送出的時間與執行者
    pub digest: String,
    /// 已固定的可升級台數
    pub eligible: usize,
    /// 其中降版的台數
    pub downgrade: usize,
    /// 不會升級的主機：原因 → 台數
    pub skipped: BTreeMap<String, usize>,
    /// 每批的台數
    pub waves: Vec<usize>,
}

impl Submission {
    /// 核准時要輸入的字串（[`rollout::approve_confirm_text`]）
    pub fn confirm_text(&self) -> String {
        rollout::approve_confirm_text(self.eligible, self.downgrade > 0)
    }

    pub fn confirm_level(&self) -> &'static str {
        if self.downgrade > 0 { "L3" } else { "L2" }
    }
}

/// `digest` 的格式版本（內容的組成改變時遞增）
const DIGEST_FORMAT: &str = "kairos.rollout.submission.v1";

/// 送審摘要。`digest` 涵蓋：推送的編號、名稱、目標版本（與版本庫中的 id）、選擇條件、批次、排程、選項
/// （資料庫中正規化後的 JSON 文字）、建立者、送出的時間與執行者，以及每台已固定主機的
/// (`agent_id`, 批次, 批次內順序, 狀態, 原因, 原本的版本)（依 `agent_id` 排序）。內容或對象有任何改變、
/// 或重新送出（`submitted_at` 不同），`digest` 就不同。只用陣列與字串組成，序列化的結果是確定的
pub fn submission(r: &RolloutRow, hosts: &[RolloutHostRow]) -> Submission {
    let target = Version::parse(&r.target_version).ok();
    let wave_count = rollout::waves_from_db(&r.waves).len();
    let mut waves = vec![0usize; wave_count];
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    let (mut eligible, mut downgrade) = (0, 0);
    for h in hosts {
        if h.state == HOST_SKIPPED {
            *skipped
                .entry(h.reason.clone().unwrap_or_else(|| "unknown".to_string()))
                .or_default() += 1;
            continue;
        }
        eligible += 1;
        if let Some(w) = usize::try_from(h.wave).ok().and_then(|w| waves.get_mut(w)) {
            *w += 1;
        }
        let from = h.from_version.as_deref().and_then(|v| Version::parse(v).ok());
        if matches!((&from, &target), (Some(c), Some(t)) if t < c) {
            downgrade += 1;
        }
    }
    let mut rows: Vec<&RolloutHostRow> = hosts.iter().collect();
    rows.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    let doc = json!([
        DIGEST_FORMAT,
        r.id,
        r.name,
        r.target_version,
        r.release_id,
        r.selector,
        r.waves,
        r.schedule,
        r.options,
        r.created_by,
        r.submitted_at,
        r.submitted_by,
        rows.iter()
            .map(|h| json!([h.agent_id, h.wave, h.ordinal, h.state, h.reason, h.from_version]))
            .collect::<Vec<_>>(),
    ]);
    Submission {
        digest: kairos_proto::enroll::sha256_hex(doc.to_string().as_bytes()),
        eligible,
        downgrade,
        skipped,
        waves,
    }
}

// ---- 誰可以核准 ----

/// 目前已知、可以核准推送的帳號（稽核的執行者格式 `user:<帳號>`）：啟用中、全站範圍有 `rollouts:approve`。
/// 目錄服務的帳號以最近一次登入時所屬的群組計算（從未登入過的不在本機資料中，不會計入）
pub fn approvers(s: &dyn Store, now_ms: i64) -> StoreResult<Vec<String>> {
    let mut out = Vec::new();
    for u in s.list_users()?.into_iter().filter(|u| u.active()) {
        let groups = if u.source == crate::users::store::SOURCE_LDAP {
            s.user_sessions(u.user_id, false, now_ms)?
                .into_iter()
                .max_by_key(|x| x.created_at)
                .map(|x| x.ldap_groups)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let (grants, _) = s.grants_for(u.user_id, &groups)?;
        if grants.has(Permission::RolloutsApprove) {
            out.push(format!("user:{}", u.username));
        }
    }
    Ok(out)
}

/// 這些執行者是不是同一個人（建立者或送出者）
fn involved(r_created_by: &str, r_submitted_by: Option<&str>, actor: &str) -> bool {
    actor == r_created_by || r_submitted_by == Some(actor)
}

/// `approvers_available`：[`approvers`] 中不是這個推送的建立者或送出者的人數
pub fn approvers_available(approvers: &[String], created_by: &str, submitted_by: Option<&str>) -> usize {
    approvers
        .iter()
        .filter(|a| !involved(created_by, submitted_by, a))
        .count()
}

/// 這次請求的身分（清單的 `pending_approval_actionable`、推送的 `viewer`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    /// 稽核的執行者（`user:<帳號>`、`token:<id>`）
    pub actor: String,
    /// 以帳號登入的工作階段（API token 不能核准）
    pub session: bool,
    /// 有 `rollouts:approve`
    pub approve: bool,
}

impl Viewer {
    /// 不能核准的原因：`forbidden`（沒有權限）、`session_required`（API token）、`self_approval`
    /// （建立者或送出者）；可以核准時 `None`。與 `POST …/approve` 的檢查順序相同
    pub fn blocked(&self, r: &RolloutRow) -> Option<&'static str> {
        if !self.approve {
            Some("forbidden")
        } else if !self.session {
            Some("session_required")
        } else if involved(&r.created_by, r.submitted_by.as_deref(), &self.actor) {
            Some("self_approval")
        } else {
            None
        }
    }

    /// 待核准的推送中這個身分可以核准的
    pub fn can_approve(&self, r: &RolloutRow) -> bool {
        r.state == rollout::ROLLOUT_PENDING_APPROVAL && self.blocked(r).is_none()
    }

    /// 推送 JSON 的 `viewer`（待核准時才有意義）
    pub fn json(&self, r: &RolloutRow) -> Value {
        json!({
            "can_approve": self.can_approve(r),
            "blocked": self.blocked(r),
            "is_creator": self.actor == r.created_by,
            "is_submitter": r.submitted_by.as_deref() == Some(self.actor.as_str()),
        })
    }
}

// ---- 主控台的 submission 物件 ----

/// [`submission_json`] 需要的目前狀態（一次請求載入一次）
#[derive(Debug, Clone, Default)]
pub struct PendingContext {
    /// 清冊（已套用連線中的 hello）：host_id → 主機
    pub inventory: HashMap<i64, UpgradeHost>,
    /// 檔案有問題的發佈檔
    pub missing: Vec<MissingFile>,
    /// 可以核准推送的帳號
    pub approvers: Vec<String>,
}

impl PendingContext {
    pub fn load(
        s: &dyn Store,
        live: &HashMap<String, super::engine::LiveHello>,
        releases_dir: &Path,
        now_ms: i64,
    ) -> StoreResult<PendingContext> {
        Ok(PendingContext {
            inventory: super::engine::with_live(s.upgrade_hosts()?, live)
                .into_iter()
                .map(|h| (h.host_id, h))
                .collect(),
            missing: missing_files(s, releases_dir)?,
            approvers: approvers(s, now_ms)?,
        })
    }
}

/// 無法自動回滾的台數：deb／rpm、版本庫沒有主機原本版本的同格式同平台檔案（依目前的版本庫計算）
fn no_rollback_count(
    ctx: &ReleaseContext,
    hosts: &[RolloutHostRow],
    inventory: &HashMap<i64, UpgradeHost>,
) -> usize {
    hosts
        .iter()
        .filter(|h| h.state != HOST_SKIPPED)
        .filter(|h| {
            let (method, arch) = method_arch(inventory.get(&h.host_id));
            matches!(method.as_str(), "deb" | "rpm")
                && !h
                    .from_version
                    .as_deref()
                    .and_then(|v| ctx.by_version.get(v))
                    .is_some_and(|list| rollout::pick_artifact(list, &method, &arch).is_some())
        })
        .count()
}

/// 待核准推送的 `submission`（ADR-0002 決策點 2b）。`digest`、`confirm_text`、`confirm_level`、`eligible`、
/// `downgrade`、`skipped`、`waves` 只由已固定的內容計算（核准時驗證的就是這些）；`no_rollback`、
/// `artifact_problems`、`warnings`、`approvers_available` 依目前的版本庫、清冊與帳號計算，每次讀取都可能不同
pub(crate) fn submission_json(
    s: &dyn Store,
    r: &RolloutRow,
    hosts: &[RolloutHostRow],
    cx: &PendingContext,
) -> Result<Value, TxError> {
    let sub = submission(r, hosts);
    let (_, ctx) = super::engine::release_context(s, &r.target_version, &cx.missing)?;
    let options = rollout::options_from_db(&r.options);
    let waves = rollout::waves_from_db(&r.waves);
    let no_rollback = no_rollback_count(&ctx, hosts, &cx.inventory);
    let problems = fixed_problems(&cx.missing, r, &options, hosts, &cx.inventory);
    let available = approvers_available(&cx.approvers, &r.created_by, r.submitted_by.as_deref());
    let mut warnings = rollout::risk_warnings(
        &waves,
        &sub.waves,
        sub.eligible,
        sub.downgrade,
        no_rollback,
        !problems.is_empty(),
    );
    if available == 0 {
        warnings.push("no_approvers");
    }
    Ok(json!({
        "digest": sub.digest,
        "confirm_text": sub.confirm_text(),
        "confirm_level": sub.confirm_level(),
        "eligible": sub.eligible,
        "downgrade": sub.downgrade,
        "no_rollback": no_rollback,
        "skipped": sub.skipped,
        "waves": sub.waves,
        "warnings": warnings,
        "artifact_problems": problems_json(&problems),
        "approvers_available": available,
        // 核准期限（待核准的逾時，ADR-0002 切片 S6）尚未實作：一律 null
        "approval_deadline": Value::Null,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout(submitted_at: i64) -> RolloutRow {
        RolloutRow {
            id: "r-20261113-0001".into(),
            name: "x".into(),
            release_id: 1,
            target_version: "0.9.2".into(),
            selector: r#"{"all":true}"#.into(),
            waves: r#"[{"size":"5%"},{"size":"rest"}]"#.into(),
            schedule: "{}".into(),
            options: "{}".into(),
            state: rollout::ROLLOUT_PENDING_APPROVAL.into(),
            current_wave: 0,
            wave_started_at: None,
            soak_until: None,
            awaiting_continue: false,
            pause_reason: None,
            pause_cause: None,
            cancel_reason: None,
            created_at: 1,
            created_by: "user:alice".into(),
            started_at: None,
            completed_at: None,
            updated_at: 1,
            submitted_at: Some(submitted_at),
            submitted_by: Some("user:bob".into()),
            approved_at: None,
            approved_by: None,
            reject_reason: None,
        }
    }

    fn host(agent: &str, wave: i64, state: &str, from: &str) -> RolloutHostRow {
        RolloutHostRow {
            rollout_id: "r-20261113-0001".into(),
            agent_id: agent.into(),
            wave,
            state: state.into(),
            reason: (state == HOST_SKIPPED).then(|| "already".to_string()),
            from_version: Some(from.into()),
            upgrade_id: format!("up-{agent}"),
            ..Default::default()
        }
    }

    #[test]
    fn digest_covers_content_hosts_and_submission_but_not_row_order_or_upgrade_ids() {
        let r = rollout(100);
        let hosts = vec![
            host("a", 0, "pending", "0.9.0"),
            host("b", 1, "pending", "0.9.1"),
            host("c", -1, "skipped", "0.9.2"),
        ];
        let s = submission(&r, &hosts);
        assert_eq!(s.digest.len(), 64);
        assert_eq!((s.eligible, s.downgrade), (2, 0));
        assert_eq!(s.waves, vec![1, 1]);
        assert_eq!(s.skipped.get("already"), Some(&1));
        assert_eq!((s.confirm_text().as_str(), s.confirm_level()), ("核准 2 台", "L2"));
        // 列的順序與 upgrade_id（隨機、暫停後會換）不影響
        let mut shuffled = hosts.clone();
        shuffled.reverse();
        shuffled[0].upgrade_id = "other".into();
        assert_eq!(submission(&r, &shuffled).digest, s.digest);
        // 內容、對象、送出的時間或送出者任何一項不同，digest 就不同
        let mut changed = Vec::new();
        let mut r2 = r.clone();
        r2.waves = r#"[{"size":"rest"}]"#.into();
        changed.push(submission(&r2, &hosts).digest);
        let mut r2 = r.clone();
        r2.name = "y".into();
        changed.push(submission(&r2, &hosts).digest);
        let mut r2 = r.clone();
        r2.schedule = r#"{"starts_at":1}"#.into();
        changed.push(submission(&r2, &hosts).digest);
        changed.push(submission(&rollout(101), &hosts).digest);
        let mut r2 = r.clone();
        r2.submitted_by = Some("user:carol".into());
        changed.push(submission(&r2, &hosts).digest);
        changed.push(submission(&r, &hosts[..2]).digest);
        let mut h2 = hosts.clone();
        h2[1].wave = 0;
        changed.push(submission(&r, &h2).digest);
        let mut h2 = hosts.clone();
        h2[2].reason = Some("no_artifact".into());
        changed.push(submission(&r, &h2).digest);
        let mut h2 = hosts.clone();
        h2[0].from_version = Some("0.9.1".into());
        changed.push(submission(&r, &h2).digest);
        for (i, d) in changed.iter().enumerate() {
            assert_ne!(d, &s.digest, "第 {i} 項");
        }
        let unique: std::collections::HashSet<_> = changed.iter().collect();
        assert_eq!(unique.len(), changed.len());
    }

    #[test]
    fn downgrades_need_the_l3_phrase() {
        let r = rollout(100);
        let hosts = vec![host("a", 0, "pending", "0.9.3"), host("b", 1, "pending", "0.9.0")];
        let s = submission(&r, &hosts);
        assert_eq!((s.eligible, s.downgrade), (2, 1));
        assert_eq!(
            (s.confirm_text().as_str(), s.confirm_level()),
            ("核准降版 2 台", "L3")
        );
    }

    #[test]
    fn viewer_rules_match_the_approve_endpoint() {
        let r = rollout(100);
        let v = |actor: &str, session, approve| Viewer {
            actor: actor.into(),
            session,
            approve,
        };
        assert_eq!(v("user:alice", true, true).blocked(&r), Some("self_approval"));
        assert_eq!(v("user:bob", true, true).blocked(&r), Some("self_approval"));
        assert_eq!(v("user:carol", true, true).blocked(&r), None);
        assert_eq!(v("user:carol", true, false).blocked(&r), Some("forbidden"));
        assert_eq!(v("token:t1", false, true).blocked(&r), Some("session_required"));
        assert!(v("user:carol", true, true).can_approve(&r));
        let mut draft = r.clone();
        draft.state = rollout::ROLLOUT_DRAFT.into();
        assert!(!v("user:carol", true, true).can_approve(&draft));
        let j = v("user:bob", true, true).json(&r);
        assert_eq!(
            (j["can_approve"].as_bool(), j["blocked"].as_str(), j["is_creator"].as_bool(), j["is_submitter"].as_bool()),
            (Some(false), Some("self_approval"), Some(false), Some(true))
        );
        let all = vec!["user:alice".to_string(), "user:bob".into(), "user:carol".into()];
        assert_eq!(approvers_available(&all, "user:alice", Some("user:bob")), 1);
        assert_eq!(approvers_available(&all[..2], "user:alice", Some("user:bob")), 0);
        assert_eq!(approvers_available(&all[..2], "user:alice", Some("user:alice")), 1);
    }

    #[test]
    fn risk_warnings_use_fixed_codes() {
        use rollout::{WaveSpec, risk_warnings};
        let two = WaveSpec::default_template();
        assert!(risk_warnings(&two, &[1, 99], 100, 0, 0, false).is_empty());
        // 第一批超過 10%
        assert_eq!(risk_warnings(&two, &[20, 80], 100, 0, 0, false), vec!["large_first_wave"]);
        // 只有一批、降版、無法自動回滾、檔案有問題
        let one = vec![two[1].clone()];
        assert_eq!(
            risk_warnings(&one, &[100], 100, 3, 2, true),
            vec!["single_wave", "downgrade", "no_rollback", "artifact_problem"]
        );
        // 只有一台主機時一批不算風險
        assert!(risk_warnings(&one, &[1], 1, 0, 0, false).is_empty());
        // 批次之間沒有停頓、某一批是 0 台
        let mut flat = two.clone();
        flat[0].soak_minutes = 0;
        flat[0].manual_continue = false;
        assert_eq!(
            risk_warnings(&flat, &[1, 0], 1, 0, 0, false),
            vec!["no_pause_between_waves", "empty_wave"]
        );
    }
}
