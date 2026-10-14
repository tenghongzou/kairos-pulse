//! API 與 CLI 共用的操作：靜音的驗證（含伺服器端的時長上限，D26）、識別碼、JSON 表示。

use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};

use super::model::{Event, Rule, Scope, Severity, Silence, SilenceKind};
use super::scope::{self, LIMIT_MAINTENANCE_SECS, SilenceLimit};
use crate::api::http::ts_ms;

/// `<前綴>-<12 個十六進位字元>`
pub fn new_id(prefix: &str) -> String {
    let mut b = [0u8; 6];
    let _ = SystemRandom::new().fill(&mut b);
    format!(
        "{prefix}-{}",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

/// 驗證失敗
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// `invalid_parameter`、`silence_too_long`、`rule_not_found`
    pub code: &'static str,
    pub field: String,
    pub message: String,
    pub details: Value,
}

fn invalid(field: &str, message: impl Into<String>) -> Invalid {
    Invalid {
        code: "invalid_parameter",
        field: field.to_string(),
        message: message.into(),
        details: Value::Null,
    }
}

/// 建立或修改靜音的輸入
#[derive(Debug, Clone, Default)]
pub struct SilenceInput {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub scope: Option<Scope>,
    pub rules: Option<Vec<String>>,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub duration_secs: Option<u64>,
    pub reason: Option<String>,
    pub notify_on_end: Option<bool>,
    pub exclude_uptime: Option<bool>,
    pub from_event: Option<String>,
}

/// 範圍選取的規則、時長上限（臨時靜音）與需要確認的原因（L1：大範圍、包含緊急）
#[derive(Debug)]
pub struct SilencePlan {
    pub limit: SilenceLimit,
    pub confirm: Vec<String>,
}

/// 依範圍與規則計算上限與需要確認的原因。`from_emergency` 為從緊急事件建立
pub fn plan(
    kind: SilenceKind,
    scope_all: bool,
    rules: &[String],
    all_rules: &[Rule],
    from_emergency: bool,
    matching_hosts: usize,
) -> Result<SilencePlan, Invalid> {
    let mut selected: Vec<&Rule> = Vec::new();
    for id in rules {
        let r = all_rules
            .iter()
            .find(|r| &r.rule_id == id)
            .ok_or_else(|| Invalid {
                code: "rule_not_found",
                ..invalid("rules", format!("沒有規則 {id}"))
            })?;
        selected.push(r);
    }
    let limit = scope::silence_limit(&selected, all_rules, from_emergency);
    let mut confirm = Vec::new();
    if matching_hosts > scope::CONFIRM_HOSTS || (scope_all && rules.is_empty()) {
        confirm
            .push("這等於關閉所有告警（範圍超過 1,000 台主機，或所有規則＋全部主機）".to_string());
    }
    if kind == SilenceKind::Silence && limit.emergency {
        confirm.push("此靜音包含緊急等級事件，期間不會通知任何人，包括升級對象".to_string());
    }
    Ok(SilencePlan { limit, confirm })
}

/// 驗證新的靜音（伺服器端檢查時長上限）。`existing` 為修改時的原始資料
#[allow(clippy::too_many_arguments)]
pub fn validate_silence(
    input: &SilenceInput,
    existing: Option<&Silence>,
    all_rules: &[Rule],
    from_event: Option<&Event>,
    matching_hosts: usize,
    actor: &str,
    now: i64,
) -> Result<(Silence, SilencePlan), Invalid> {
    let kind = match (&input.kind, existing) {
        (Some(k), None) => SilenceKind::parse(k).ok_or_else(|| {
            invalid(
                "kind",
                "kind 必須是 silence（臨時靜音）或 maintenance（維護時段）",
            )
        })?,
        (None, None) => SilenceKind::Silence,
        (_, Some(e)) => e.kind,
    };
    let name = input
        .name
        .clone()
        .or_else(|| existing.map(|e| e.name.clone()))
        .ok_or_else(|| invalid("name", "name 必填"))?;
    let name = name.trim().to_string();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(invalid("name", "名稱必須是 1～100 個字元"));
    }
    let reason = input
        .reason
        .clone()
        .or_else(|| existing.map(|e| e.reason.clone()))
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .ok_or_else(|| invalid("reason", "原因必填（寫入稽核紀錄）"))?;
    if reason.chars().count() > 500 {
        return Err(invalid("reason", "原因最多 500 個字元"));
    }
    let scope_in = input
        .scope
        .clone()
        .or_else(|| existing.map(|e| e.scope.clone()))
        .unwrap_or_default();
    let scope = scope::normalize(&scope_in).map_err(|(f, m)| invalid(f, m))?;
    let mut rules: Vec<String> = input
        .rules
        .clone()
        .or_else(|| existing.map(|e| e.rules.clone()))
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    rules.sort();
    rules.dedup();
    let from_emergency = from_event.is_some_and(|e| e.severity == Severity::Emergency);
    let plan = plan(
        kind,
        scope.is_all(),
        &rules,
        all_rules,
        from_emergency,
        matching_hosts,
    )?;
    let starts_at = input
        .starts_at
        .or_else(|| existing.map(|e| e.starts_at))
        .unwrap_or(now);
    if existing.is_none() && starts_at < now - 5 * 60_000 {
        return Err(invalid("starts_at", "開始時間不可早於現在"));
    }
    let ends_at = match (input.ends_at, input.duration_secs) {
        (Some(_), Some(_)) => return Err(invalid("ends_at", "ends_at 與 duration_secs 只能擇一")),
        (Some(e), None) => e,
        (None, Some(d)) => starts_at + d as i64 * 1000,
        (None, None) => existing
            .map(|e| e.ends_at)
            .ok_or_else(|| invalid("ends_at", "必須設定結束時間（ends_at 或 duration_secs）"))?,
    };
    if ends_at <= starts_at || ends_at <= now {
        return Err(invalid("ends_at", "結束時間必須晚於開始時間與現在"));
    }
    let length = ((ends_at - starts_at) / 1000) as u64;
    let max = match kind {
        SilenceKind::Silence => plan.limit.max_secs,
        SilenceKind::Maintenance => LIMIT_MAINTENANCE_SECS,
    };
    if length > max {
        return Err(Invalid {
            code: "silence_too_long",
            field: "ends_at".into(),
            message: match kind {
                SilenceKind::Silence => format!(
                    "臨時靜音最長 {}：{}",
                    scope::duration_text(max),
                    plan.limit.reason
                ),
                SilenceKind::Maintenance => "一次性的維護時段最長 366 天".to_string(),
            },
            details: json!({
                "max_secs": max,
                "requested_secs": length,
                "system": plan.limit.system,
                "emergency": plan.limit.emergency,
            }),
        });
    }
    let s = Silence {
        silence_id: existing.map_or_else(|| new_id("s"), |e| e.silence_id.clone()),
        kind,
        name,
        scope,
        rules,
        starts_at,
        ends_at,
        reason,
        notify_on_end: input
            .notify_on_end
            .or_else(|| existing.map(|e| e.notify_on_end))
            .unwrap_or(true),
        exclude_uptime: input
            .exclude_uptime
            .or_else(|| existing.map(|e| e.exclude_uptime))
            .unwrap_or(false),
        from_event: existing
            .and_then(|e| e.from_event.clone())
            .or_else(|| from_event.map(|e| e.event_id.clone())),
        created_at: existing.map_or(now, |e| e.created_at),
        created_by: existing.map_or_else(|| actor.to_string(), |e| e.created_by.clone()),
        updated_at: now,
        updated_by: actor.to_string(),
        ended_at: existing.and_then(|e| e.ended_at),
        ended_by: existing.and_then(|e| e.ended_by.clone()),
        end_reason: existing.and_then(|e| e.end_reason.clone()),
        end_notified: existing.is_some_and(|e| e.end_notified),
    };
    Ok((s, plan))
}

/// 靜音的 JSON（API、CLI）
pub fn silence_json(s: &Silence, now: i64, matching_hosts: Option<usize>) -> Value {
    json!({
        "silence_id": s.silence_id,
        "kind": s.kind,
        "name": s.name,
        "status": s.status(now),
        "scope": s.scope,
        "scope_text": scope::sentence(&s.scope),
        "scope_short": scope::short(&s.scope),
        "rules": s.rules,
        "matching_hosts": matching_hosts,
        "starts_at": ts_ms(s.starts_at),
        "ends_at": ts_ms(s.ends_at),
        "effective_end": ts_ms(s.effective_end()),
        "remaining_secs": s.is_active(now).then(|| (s.effective_end() - now) / 1000),
        "reason": s.reason,
        "notify_on_end": s.notify_on_end,
        "exclude_uptime": s.exclude_uptime,
        "from_event": s.from_event,
        "created_at": ts_ms(s.created_at),
        "created_by": s.created_by,
        "updated_at": ts_ms(s.updated_at),
        "updated_by": s.updated_by,
        "ended_at": s.ended_at.map(ts_ms),
        "ended_by": s.ended_by,
        "end_reason": s.end_reason,
    })
}

/// 事件的 JSON（列表與詳情；`summary_json` 之外另有內容與處理狀態）
pub fn event_json(e: &Event) -> Value {
    let mut v = e.summary_json();
    if let Value::Object(m) = &mut v {
        m.insert("details".into(), e.details.clone());
        m.insert("acked_at".into(), json!(e.acked_at.map(ts_ms)));
        m.insert("acked_by".into(), json!(e.acked_by));
        m.insert("eta_at".into(), json!(e.eta_at.map(ts_ms)));
        m.insert("root_cause".into(), json!(e.root_cause));
        m.insert(
            "root_cause_label".into(),
            json!(e.root_cause.map(|c| c.label())),
        );
        m.insert("resolve_note".into(), json!(e.resolve_note));
        m.insert("resolved_by".into(), json!(e.resolved_by));
        m.insert("snooze_until".into(), json!(e.snooze_until.map(ts_ms)));
        m.insert("last_signal_at".into(), json!(ts_ms(e.last_signal_at)));
        m.insert(
            "notifications".into(),
            json!({
                "sent": e.notify.sends,
                "last_sent_at": e.notify.last_sent_at.map(ts_ms),
                "escalated": e.notify.escalated,
            }),
        );
        let attention_text = match e.attention {
            super::model::Attention::Advisory => Some("規劃提醒：不需要確認，條件解除時自動結束"),
            super::model::Attention::Record => Some("紀錄：不需要確認"),
            super::model::Attention::Action => None,
        };
        m.insert("attention_text".into(), json!(attention_text));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::model::Category;
    use crate::alerts::rules;

    fn input(kind: &str, rules: &[&str], secs: u64) -> SilenceInput {
        SilenceInput {
            kind: Some(kind.into()),
            name: Some("歲修".into()),
            rules: Some(rules.iter().map(|r| r.to_string()).collect()),
            duration_secs: Some(secs),
            reason: Some("年度歲修".into()),
            scope: Some(Scope {
                tags: vec!["產線".into()],
                ..Scope::default()
            }),
            ..SilenceInput::default()
        }
    }

    #[test]
    fn server_side_duration_limits() {
        let all = rules::builtins();
        let now = 1_000_000;
        // 一般規則（主機長期失聯為警告）：7 天可以、超過不行
        let ok = validate_silence(
            &input("silence", &[rules::ids::HOST_LONG_OFFLINE], 7 * 86_400),
            None,
            &all,
            None,
            3,
            "t",
            now,
        );
        assert!(ok.is_ok());
        let e = validate_silence(
            &input("silence", &[rules::ids::HOST_LONG_OFFLINE], 7 * 86_400 + 1),
            None,
            &all,
            None,
            3,
            "t",
            now,
        )
        .unwrap_err();
        assert_eq!(e.code, "silence_too_long");
        assert_eq!(e.details["max_secs"], 7 * 86_400);
        // 系統規則：24 小時
        let e = validate_silence(
            &input("silence", &[rules::ids::RETENTION_PAUSED], 25 * 3600),
            None,
            &all,
            None,
            3,
            "t",
            now,
        )
        .unwrap_err();
        assert_eq!(e.details["max_secs"], 24 * 3600);
        // 只選 CRL 即將到期（系統＋緊急）：4 小時
        let e = validate_silence(
            &input("silence", &[rules::ids::CRL_EXPIRING], 5 * 3600),
            None,
            &all,
            None,
            3,
            "t",
            now,
        )
        .unwrap_err();
        assert_eq!(e.details["max_secs"], 4 * 3600);
        assert!(
            e.message.contains("24 小時") && e.message.contains("4 小時"),
            "{}",
            e.message
        );
        // 不選規則＝所有規則（含緊急）：4 小時
        let e = validate_silence(
            &input("silence", &[], 5 * 3600),
            None,
            &all,
            None,
            3,
            "t",
            now,
        )
        .unwrap_err();
        assert_eq!(e.details["max_secs"], 4 * 3600);
        // 維護時段不受上限
        assert!(
            validate_silence(
                &input("maintenance", &[], 3 * 86_400),
                None,
                &all,
                None,
                3,
                "t",
                now
            )
            .is_ok()
        );
        // 不存在的規則
        assert_eq!(
            validate_silence(
                &input("silence", &["nope"], 60),
                None,
                &all,
                None,
                3,
                "t",
                now
            )
            .unwrap_err()
            .code,
            "rule_not_found"
        );
        // 確認：大範圍、包含緊急
        let (_, plan) = validate_silence(
            &input("silence", &[rules::ids::SUBNET_OFFLINE], 3600),
            None,
            &all,
            None,
            1500,
            "t",
            now,
        )
        .unwrap();
        assert_eq!(plan.confirm.len(), 2);
        assert_eq!(
            all.iter()
                .find(|r| r.rule_id == rules::ids::SUBNET_OFFLINE)
                .unwrap()
                .category,
            Category::Aggregate
        );
        // 原因必填
        let mut no_reason = input("silence", &[], 60);
        no_reason.reason = None;
        assert_eq!(
            validate_silence(&no_reason, None, &all, None, 3, "t", now)
                .unwrap_err()
                .field,
            "reason"
        );
    }
}
