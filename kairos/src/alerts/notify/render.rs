//! 通知內容的範本（設計包 `alerts-ux.md` 第 6 節）：Email 的主旨與純文字內文、Webhook 的各種格式
//! （Kairos JSON `kairos.alert.v1`、Slack 類的 `{"text"}`、Teams Workflows 的 Adaptive Card、
//! LINE Messaging API 的 push message）。
//!
//! - 主旨／第一行：`【等級】事件類型：對象 — 一句話摘要`，手機鎖定畫面只看得到這一行。
//! - 時間一律以管道設定的時區顯示並寫出時區（`2026-10-09 02:13 UTC+8`）。
//! - 不放敏感資訊：沒有憑證指紋與 token；IP 依管道的 `show_ip`（`full`／`subnet`／`none`），
//!   對外部服務（Slack、Teams、LINE 格式）預設只顯示網段。

use chrono::{DateTime, FixedOffset};
use serde_json::{Value, json};

use crate::alerts::model::Severity;

/// 給人看的訊息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub subject: String,
    pub text: String,
    pub severity: Severity,
}

/// 管道的顯示選項
#[derive(Debug, Clone)]
pub struct View {
    pub offset: FixedOffset,
    /// `full`、`subnet`、`none`
    pub show_ip: String,
    pub channel_name: String,
}

/// `UTC+8`、`UTC-3:30`、`UTC`
pub fn zone(offset: FixedOffset) -> String {
    crate::token_cli::zone_label(offset)
}

/// `2026-10-09 02:13 UTC+8`
pub fn time(ms: i64, offset: FixedOffset) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|t| {
            format!(
                "{} {}",
                t.with_timezone(&offset).format("%Y-%m-%d %H:%M"),
                zone(offset)
            )
        })
        .unwrap_or_default()
}

/// `7 分鐘`、`2 小時 5 分`、`3 天 4 小時`
pub fn duration(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..60 => format!("{s} 秒"),
        60..3600 => format!("{} 分鐘", s / 60),
        3600..86_400 => format!("{} 小時 {} 分", s / 3600, s % 3600 / 60),
        _ => format!("{} 天 {} 小時", s / 86_400, s % 86_400 / 3600),
    }
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn i(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

/// 依 `show_ip` 顯示位址
fn ip_text(details: &Value, view: &View) -> Option<String> {
    match view.show_ip.as_str() {
        "none" => None,
        "subnet" => s(details, "subnet").map(str::to_string),
        _ => s(details, "ip").map(str::to_string),
    }
}

/// 產生訊息（`payload` 為引擎排入的內容，見 `engine::Engine::payload`；`now` 用於持續時間）
pub fn message(payload: &Value, view: &View, now: i64) -> Message {
    let kind = s(payload, "kind").unwrap_or("firing");
    match kind {
        "silence_end" => return silence_end(payload, view),
        "digest" => return digest(payload, view),
        "channel_recovered" => return channel_recovered(payload),
        _ => {}
    }
    let ev = payload.get("event").cloned().unwrap_or(Value::Null);
    let details = ev.get("details").cloned().unwrap_or(Value::Null);
    let sev = s(&ev, "severity")
        .and_then(Severity::parse)
        .unwrap_or(Severity::Warning);
    let label = sev.label();
    let title = s(&ev, "title").unwrap_or("告警");
    let started = i(&ev, "started_at").unwrap_or(now);
    let event_id = s(&ev, "event_id").unwrap_or("");
    let extra = payload.get("extra").cloned().unwrap_or(Value::Null);
    let lasting = duration(now - started);
    let subject = match kind {
        "repeat" => format!(
            "【{label}】{title} — 已持續 {lasting}（第 {} 次通知）",
            i(payload, "repeat").unwrap_or(1) + 1
        ),
        "escalation" => format!("【升級】【{label}】{title} — 已 {lasting} 無人確認"),
        "reopen" => format!(
            "【{label}】再次發生（第 {} 次）：{title}",
            i(&extra, "occurrence").unwrap_or(2)
        ),
        "update" => format!("【{label}】更新：{title}"),
        "recovery" => {
            let rec = i(&ev, "recovered_at").unwrap_or(now);
            format!("【已恢復】{title} — 共持續 {}", duration(rec - started))
        }
        "ack" => format!(
            "【{label}】已確認：{title} — {} 處理中{}",
            s(&extra, "by").unwrap_or("有人"),
            i(&extra, "eta_minutes")
                .map(|m| format!("（預計 {m} 分鐘）"))
                .unwrap_or_default()
        ),
        "flapping" => format!(
            "【{label}】不穩定：{title}（30 分鐘內 {} 次狀態變化）",
            i(&extra, "transitions_30m").unwrap_or(0)
        ),
        "detached" => format!("【{label}】{title}（彙總事件已恢復，這台仍未恢復）"),
        "test" => format!("【{label}】測試通知：{}", view.channel_name),
        _ => {
            if s(&ev, "kind") == Some("host_offline") {
                let off = i(&details, "offline_at").unwrap_or(started);
                format!("【{label}】{title} — 已離線 {}", duration(now - off))
            } else {
                format!("【{label}】{title}")
            }
        }
    };
    let mut lines = Vec::new();
    lines.push(title.to_string());
    lines.push(String::new());
    lines.push(format!("  事件編號    {event_id}"));
    lines.push(format!("  等級        {label}"));
    if let Some(state) = s(&ev, "state") {
        let st = crate::alerts::model::EventState::parse(state).map_or(state, |x| x.label());
        lines.push(format!("  狀態        {st}"));
    }
    if let Some(h) = s(&details, "hostname") {
        lines.push(format!("  主機        {h}"));
    }
    if let Some(ip) = ip_text(&details, view) {
        let what = if view.show_ip == "subnet" {
            "網段"
        } else {
            "位址"
        };
        lines.push(format!("  {what}        {ip}"));
    }
    if let Some(sub) = s(&details, "subnet").filter(|_| view.show_ip == "full") {
        lines.push(format!("  網段        {sub}"));
    }
    lines.push(format!("  開始        {}", time(started, view.offset)));
    if let Some(r) = s(&details, "offline_reason_label") {
        lines.push(format!(
            "  中斷原因    {r}（{}）",
            s(&details, "offline_reason").unwrap_or("")
        ));
    }
    if let Some(ms) = i(&details, "offline_after_ms") {
        lines.push(format!(
            "  判定門檻    {} 秒未收到心跳即判定離線",
            ms / 1000
        ));
    }
    if let Some(v) = details.get("value").and_then(Value::as_f64) {
        lines.push(format!(
            "  目前數值    {v:.1}%（門檻 {} {}%）",
            s(&details, "op").unwrap_or(">"),
            details
                .get("threshold")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        ));
    }
    if let Some(code) = s(&ev, "code") {
        lines.push(format!("  錯誤代碼    {code}"));
    }
    let scope = payload.get("scope").cloned().unwrap_or(Value::Null);
    let total = i(&scope, "total").unwrap_or(0);
    if total > 0 {
        let count = i(&scope, "count").unwrap_or(0);
        lines.push(format!("  受影響      {count} / {total} 台仍未恢復"));
        let names: Vec<String> = scope
            .get("sample")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|h| s(h, "hostname").map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !names.is_empty() {
            let more = count - names.len() as i64;
            lines.push(format!(
                "  主機        {}{}",
                names.join("、"),
                if more > 0 {
                    format!(" 等其餘 {more} 台")
                } else {
                    String::new()
                }
            ));
        }
    }
    if let Some(hint) = s(payload, "hint") {
        lines.push(String::new());
        lines.push("說明與建議下一步".to_string());
        lines.push(format!("  {hint}"));
    }
    if let Some(link) = s(payload, "link") {
        lines.push(String::new());
        lines.push("查看事件並確認：".to_string());
        lines.push(link.to_string());
    }
    lines.push(String::new());
    lines.push("—".to_string());
    let rule = ev.get("rule_name").and_then(Value::as_str).unwrap_or("");
    let n = payload.get("notify").cloned().unwrap_or(Value::Null);
    let repeat = match (i(&n, "repeat_interval_secs"), i(&n, "repeat_max")) {
        (Some(sec), Some(max)) if max > 0 => {
            format!("未確認時每 {} 分鐘再通知一次，最多 {max} 次。", sec / 60)
        }
        _ => String::new(),
    };
    lines.push(format!(
        "這則通知由 Kairos 告警規則「{rule}」發出。{repeat}"
    ));
    Message {
        subject,
        text: lines.join("\n"),
        severity: sev,
    }
}

fn silence_end(payload: &Value, view: &View) -> Message {
    let si = payload.get("silence").cloned().unwrap_or(Value::Null);
    let name = s(&si, "name").unwrap_or("靜音");
    let what = if s(&si, "kind") == Some("maintenance") {
        "維護時段"
    } else {
        "靜音"
    };
    let count = i(payload, "count").unwrap_or(0);
    let mut lines = vec![
        format!("{what}「{name}」已結束，仍有 {count} 個事件未恢復："),
        String::new(),
    ];
    for e in payload
        .get("sample")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        lines.push(format!(
            "  {}  {}",
            s(e, "event_id").unwrap_or(""),
            s(e, "title").unwrap_or("")
        ));
    }
    if count > 10 {
        lines.push(format!("  …等其餘 {} 個", count - 10));
    }
    if let Some(end) = i(&si, "ends_at") {
        lines.push(String::new());
        lines.push(format!("結束時間：{}", time(end, view.offset)));
    }
    Message {
        subject: format!("【提醒】{what}「{name}」已結束，仍有 {count} 個事件未恢復"),
        text: lines.join("\n"),
        severity: Severity::Warning,
    }
}

/// 速率限制期間累積的多則通知合併成一則（§3.4「另有 N 則」）
fn digest(payload: &Value, view: &View) -> Message {
    let items = payload
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let msgs: Vec<Message> = items
        .iter()
        .map(|p| message(p, view, i(payload, "now").unwrap_or(0)))
        .collect();
    let top = msgs
        .iter()
        .map(|m| m.severity)
        .min()
        .unwrap_or(Severity::Warning);
    let first = msgs.first().map_or(String::new(), |m| m.subject.clone());
    let n = msgs.len();
    let mut lines = vec![format!("{n} 則通知："), String::new()];
    for m in msgs.iter().take(super::super::engine::MAX_LISTED) {
        lines.push(format!("  • {}", m.subject));
    }
    if n > super::super::engine::MAX_LISTED {
        lines.push(format!(
            "  …等其餘 {} 則",
            n - super::super::engine::MAX_LISTED
        ));
    }
    if let Some(link) = s(payload, "link").filter(|l| !l.is_empty()) {
        lines.push(String::new());
        lines.push(format!("查看清單：{link}/alerts/events"));
    }
    Message {
        subject: format!("{first}（另有 {} 則）", n.saturating_sub(1)),
        text: lines.join("\n"),
        severity: top,
    }
}

fn channel_recovered(payload: &Value) -> Message {
    let n = i(payload, "missed").unwrap_or(0);
    let mut lines = vec![format!(
        "通知管道已恢復。管道失敗期間有 {n} 則通知沒有逐則補發，相關事件如下："
    )];
    for e in payload
        .get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(super::super::engine::MAX_LISTED)
    {
        lines.push(format!(
            "  {}  {}",
            s(e, "event_id").unwrap_or(""),
            s(e, "title").unwrap_or("")
        ));
    }
    Message {
        subject: format!("【資訊】通知管道已恢復：期間 {n} 則通知未送出"),
        text: lines.join("\n"),
        severity: Severity::Info,
    }
}

/// RFC 3339 UTC
fn rfc3339(ms: Option<i64>) -> Value {
    ms.map_or(Value::Null, |m| json!(crate::api::http::ts_ms(m)))
}

/// Webhook 的本文（`format`：`kairos`、`slack`、`teams`、`line`）
pub fn webhook_body(format: &str, payload: &Value, view: &View, line_to: &str, now: i64) -> Value {
    let msg = message(payload, view, now);
    match format {
        "slack" => json!({ "text": format!("{}\n\n{}", msg.subject, msg.text) }),
        "line" => {
            let mut text = format!("{}\n\n{}", msg.subject, msg.text);
            // LINE 單則上限 5,000 字元
            if text.chars().count() > 4900 {
                text = text.chars().take(4900).collect::<String>() + "…";
            }
            json!({ "to": line_to, "messages": [{ "type": "text", "text": text }] })
        }
        "teams" => teams_card(&msg, payload),
        _ => kairos_json(payload, &msg, view),
    }
}

/// Teams Workflows 的 Adaptive Card（§6.5）：等級以文字與 `color` 雙重表示
fn teams_card(msg: &Message, payload: &Value) -> Value {
    let color = match msg.severity {
        Severity::Emergency | Severity::Critical => "Attention",
        Severity::Warning => "Warning",
        Severity::Info => "Default",
    };
    let mut body = Vec::new();
    if msg.severity == Severity::Emergency {
        body.push(json!({
            "type": "TextBlock", "text": "需要立即處理：未確認時將通知升級對象",
            "weight": "Bolder", "color": "Attention", "wrap": true
        }));
    }
    body.push(json!({
        "type": "TextBlock", "text": msg.subject, "weight": "Bolder",
        "size": if msg.severity == Severity::Emergency { "Large" } else { "Medium" },
        "color": color, "wrap": true
    }));
    body.push(json!({ "type": "TextBlock", "text": msg.text, "wrap": true }));
    let mut card = json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": body,
    });
    if let Some(link) = s(payload, "link") {
        card["actions"] = json!([{ "type": "Action.OpenUrl", "title": "查看事件", "url": link }]);
    }
    json!({
        "type": "message",
        "attachments": [{ "contentType": "application/vnd.microsoft.card.adaptive", "content": card }]
    })
}

/// `kairos.alert.v1`（§6.6）
fn kairos_json(payload: &Value, msg: &Message, view: &View) -> Value {
    let ev = payload.get("event").cloned().unwrap_or(Value::Null);
    let details = ev.get("details").cloned().unwrap_or(Value::Null);
    let scope = payload.get("scope").cloned().unwrap_or(Value::Null);
    let mut hosts: Vec<Value> = Vec::new();
    if let Some(a) = ev.get("agent_id").and_then(Value::as_str) {
        let mut h = json!({
            "agent_id": a,
            "hostname": s(&details, "hostname"),
            "offline_reason": s(&details, "offline_reason"),
        });
        if let Some(ip) = ip_text(&details, view) {
            h["address"] = json!(ip);
        }
        hosts.push(h);
    }
    for h in scope
        .get("sample")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut x = json!({
            "agent_id": h.get("agent_id"),
            "hostname": h.get("hostname"),
            "offline_reason": h.get("offline_reason"),
        });
        if view.show_ip == "full"
            && let Some(ip) = h.get("ip").filter(|v| !v.is_null())
        {
            x["address"] = ip.clone();
        }
        hosts.push(x);
    }
    let state = match s(payload, "kind") {
        Some("recovery") => "recovered",
        Some("ack") => "acknowledged",
        _ => s(&ev, "state").unwrap_or("firing"),
    };
    json!({
        "schema": "kairos.alert.v1",
        "notification": s(payload, "kind"),
        "event_id": ev.get("event_id"),
        "state": state,
        "severity": ev.get("severity"),
        "attention": ev.get("attention").cloned().unwrap_or(json!("action")),
        "kind": ev.get("kind"),
        "rule_id": ev.get("rule_id"),
        "code": ev.get("code"),
        "title": msg.subject,
        "summary": ev.get("title"),
        "started_at": rfc3339(i(&ev, "started_at")),
        "updated_at": rfc3339(i(&ev, "updated_at")),
        "recovered_at": rfc3339(i(&ev, "recovered_at")),
        "scope": {
            "subnet": if view.show_ip == "none" { Value::Null } else { details.get("subnet").cloned().unwrap_or(Value::Null) },
            "count": scope.get("count"),
            "total": scope.get("total"),
        },
        "hosts": hosts,
        "parent_event_id": ev.get("parent_id"),
        "link": payload.get("link"),
        "text": msg.text,
    })
}

/// 測試通知的內容（S11「寄送測試通知使用真實範本與假資料」）
pub fn test_payload(severity: Severity, console_url: &str, now: i64) -> Value {
    let base = console_url.trim().trim_end_matches('/');
    json!({
        "kind": "test",
        "event": {
            "event_id": "E-TEST-0001",
            "rule_id": "sys.host_offline",
            "rule_name": "主機離線",
            "kind": "host_offline",
            "category": "host",
            "severity": severity,
            "attention": "action",
            "state": "firing",
            "title": "測試通知：這不是真的事件（hmi-test-01 離線）",
            "subject": "00000000-0000-4000-8000-000000000000",
            "agent_id": "00000000-0000-4000-8000-000000000000",
            "code": null,
            "parent_id": null,
            "started_at": now - 5 * 60_000,
            "updated_at": now,
            "details": {
                "hostname": "hmi-test-01",
                "ip": "192.0.2.10",
                "subnet": "192.0.2.0/24",
                "offline_reason": "idle_timeout",
                "offline_reason_label": "連線逾時（未收到心跳）",
                "offline_at": now - 5 * 60_000,
                "offline_after_ms": 25_000,
            },
        },
        "hint": "這是從 Kairos 送出的測試通知；收到即表示這個管道可以送達。",
        "repeat": 0,
        "scope": { "count": 0, "total": 0, "sample": [] },
        "notify": {},
        "link": if base.is_empty() { Value::Null } else { json!(format!("{base}/alerts/events/E-TEST-0001")) },
        "extra": {},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(show_ip: &str) -> View {
        View {
            offset: FixedOffset::east_opt(8 * 3600).unwrap(),
            show_ip: show_ip.into(),
            channel_name: "值班LINE".into(),
        }
    }

    #[test]
    fn subject_line_follows_the_template() {
        let now = 1_760_000_000_000;
        let p = test_payload(Severity::Critical, "https://m.example/", now);
        let m = message(&p, &view("full"), now);
        assert_eq!(m.subject, "【嚴重】測試通知：值班LINE");
        assert!(m.text.contains("E-TEST-0001"));
        assert!(m.text.contains("UTC+8"), "{}", m.text);
        assert!(m.text.contains("192.0.2.10"));
        assert!(
            m.text
                .contains("https://m.example/alerts/events/E-TEST-0001")
        );
        // 外部服務只顯示網段
        let sub = message(&p, &view("subnet"), now);
        assert!(!sub.text.contains("192.0.2.10") && sub.text.contains("192.0.2.0/24"));
        let mut firing = p.clone();
        firing["kind"] = json!("firing");
        let m = message(&firing, &view("full"), now);
        assert!(
            m.subject.starts_with("【嚴重】測試通知：這不是真的事件"),
            "{}",
            m.subject
        );
        assert!(m.subject.ends_with("已離線 5 分鐘"), "{}", m.subject);
        firing["kind"] = json!("escalation");
        assert!(
            message(&firing, &view("full"), now)
                .subject
                .starts_with("【升級】【嚴重】")
        );
        firing["kind"] = json!("recovery");
        firing["event"]["recovered_at"] = json!(now);
        assert!(
            message(&firing, &view("full"), now)
                .subject
                .starts_with("【已恢復】")
        );
    }

    #[test]
    fn webhook_formats() {
        let now = 1_760_000_000_000;
        let p = test_payload(Severity::Emergency, "", now);
        let k = webhook_body("kairos", &p, &view("full"), "", now);
        assert_eq!(k["schema"], "kairos.alert.v1");
        assert_eq!(k["severity"], "emergency");
        assert_eq!(k["attention"], "action");
        assert_eq!(k["hosts"][0]["offline_reason"], "idle_timeout");
        assert!(k["started_at"].as_str().unwrap().ends_with('Z'));
        let t = webhook_body("teams", &p, &view("subnet"), "", now);
        assert_eq!(t["type"], "message");
        let card = &t["attachments"][0]["content"];
        assert_eq!(card["body"][1]["color"], "Attention");
        assert_eq!(card["body"][1]["size"], "Large");
        let l = webhook_body("line", &p, &view("subnet"), "Cabc", now);
        assert_eq!(l["to"], "Cabc");
        assert!(
            l["messages"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("【緊急】")
        );
        let s = webhook_body("slack", &p, &view("none"), "", now);
        assert!(!s["text"].as_str().unwrap().contains("192.0.2"));
    }

    #[test]
    fn durations() {
        assert_eq!(duration(42_000), "42 秒");
        assert_eq!(duration(7 * 60_000), "7 分鐘");
        assert_eq!(duration(2 * 3_600_000 + 5 * 60_000), "2 小時 5 分");
        assert_eq!(duration(3 * 86_400_000 + 4 * 3_600_000), "3 天 4 小時");
    }
}
