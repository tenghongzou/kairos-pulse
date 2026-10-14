//! `kairos alerts`、`kairos silences`、`kairos alert-rules`、`kairos alert-channels`
//! （`kairos/ALERTS.md` 第 8 節；沒有網頁介面時也能處理告警）。
//!
//! 風格依設計包 `cli-style.md`：結果在 stdout、過程與錯誤在 stderr；`--json` 為純 ASCII 的單一文件
//! （`schema` 為 `kairos.<命令>.<動作>/v1`），錯誤也以 JSON 寫到 stdout。以服務帳號執行
//! （`sudo -u kairos`），Kairos 執行中也可以：
//!
//! - 事件的確認、解決、註解交給執行中的 Kairos 處理（`alert_actions` 佇列，引擎每 2 秒處理一次），
//!   CLI 最多等 10 秒；Kairos 沒有執行時排在佇列中，下次啟動時處理（結束代碼 7）。
//! - 靜音、規則的啟停直接寫入資料庫，執行中的 Kairos 在 5 秒內重新載入。靜音的時長上限與 API 相同
//!   （D26，伺服器端驗證）；需要確認的靜音（大範圍、包含緊急）為 L1：互動時詢問，非互動需 `--yes`。

use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, ErrorKind, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::alerts::model::{Scope, Severity, SilenceKind};
use crate::alerts::ops::{self, SilenceInput};
use crate::alerts::scope::{self, Compiled, HostFacts};
use crate::alerts::store;
use crate::config::take_config_arg;
use crate::hosts_cli::{ascii_json, parse_duration, use_unicode};
use crate::storage::fmt_time;
use crate::token_cli::{
    Failure, Output, db_failure, human_time, open, operator, relative, table, zone_label,
};

const EXIT_GENERAL: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_REJECTED: u8 = 6;
const EXIT_WAITING: u8 = 7;

/// `kairos alerts --help`
pub const ALERTS_USAGE: &str = "kairos alerts — 告警事件（S06、S07；kairos/ALERTS.md）

用法：
  kairos alerts list [--view active|needs_me|recovered|all] [--severity <等級>]... [--limit <N>]
  kairos alerts show <事件編號>
  kairos alerts ack <事件編號>... [--assign-to-me] [--eta <分鐘>]
  kairos alerts resolve <事件編號> --root-cause <根因> [--note <說明>]
  kairos alerts comment <事件編號> --text <註解>

根因：hardware、network、unregistered_maintenance、software、false_positive、other。
確認、解決、註解由執行中的 Kairos 處理（最多等 10 秒）；Kairos 沒有執行時排入佇列（結束代碼 7）。

通用選項：--config <path>、--json、--ascii、-h/--help
結束代碼：0 完成、1 資料庫錯誤、2 用法錯誤、3 設定錯誤、6 被拒絕（事件已解決等）、7 已排入佇列、
10 權限不足、11 資料庫使用中";

/// `kairos silences --help`
pub const SILENCES_USAGE: &str = "kairos silences — 靜音與維護時段（S10）

用法：
  kairos silences list [--status active|upcoming|ended|all]
  kairos silences create --name <名稱> --reason <原因> (--duration <時間> | --until <時間>)
                         [--start <時間>] [--maintenance] [--rule <rule_id>]...
                         [--tag <標籤>]... [--subnet <CIDR>]... [--host <agent_id>]...
                         [--from-event <事件編號>] [--no-end-notify] [--yes]
  kairos silences end <silence_id> [--reason <原因>]

範圍：同一欄位的多個值取「或」，不同欄位之間取「且」；都不填＝全部主機。不選規則＝所有規則
（以主機為範圍的靜音不抑制系統事件；要靜音系統事件請以 --rule 明確選取）。
臨時靜音最長 7 天；包含系統規則 24 小時；包含緊急等級的規則（或從緊急事件建立）4 小時，同時符合時取最短。
維護時段（--maintenance）不受上限，但不抑制系統事件。範圍超過 1,000 台主機或包含緊急等級時需要確認：
互動時詢問，非互動需 --yes。時間：--duration 30m、4h、2d；--until/--start 為 RFC 3339 或 2026-10-20T08:00（本機時區）。

通用選項：--config <path>、--json、--ascii、-h/--help
結束代碼：0 完成、1 資料庫錯誤、2 用法錯誤（含超過時長上限、非互動缺少 --yes）、10 權限不足、11 資料庫使用中";

/// `kairos alert-rules --help`
pub const RULES_USAGE: &str = "kairos alert-rules — 告警規則（S08）

用法：
  kairos alert-rules list
  kairos alert-rules enable <rule_id>
  kairos alert-rules disable <rule_id> [--reason <原因>] [--yes]

停用系統預設規則必須填寫原因（L1：互動時詢問，非互動需 --yes）。建立與編輯規則請用主控台 API
（POST /api/v1/alert-rules）。

通用選項：--config <path>、--json、--ascii、-h/--help
結束代碼：0 完成、1 資料庫錯誤、2 用法錯誤、6 被拒絕、10 權限不足、11 資料庫使用中";

/// `kairos alert-channels --help`
pub const CHANNELS_USAGE: &str = "kairos alert-channels — 通知管道（S11）

用法：
  kairos alert-channels list

機密（SMTP 密碼、Webhook 網址與 token）只顯示遮蔽值。新增、修改與寄送測試通知請用主控台 API
（/api/v1/channels）。

通用選項：--config <path>、--json、--ascii、-h/--help";

/// 命令的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Alerts,
    Silences,
    Rules,
    Channels,
}

/// 解析後的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub group: Group,
    pub args: Vec<String>,
    pub opts: Vec<(String, String)>,
    pub flags: Vec<String>,
    pub config_path: Option<PathBuf>,
    pub json: bool,
    pub ascii: bool,
    pub help: bool,
}

const FLAGS: [&str; 6] = [
    "--yes",
    "--assign-to-me",
    "--maintenance",
    "--no-end-notify",
    "--json",
    "--ascii",
];

impl Cmd {
    fn opt(&self, name: &str) -> Option<&str> {
        self.opts
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn opts_all(&self, name: &str) -> Vec<String> {
        self.opts
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    fn sub(&self) -> Option<&str> {
        self.args.first().map(String::as_str)
    }

    fn schema(&self) -> String {
        let group = match self.group {
            Group::Alerts => "alerts",
            Group::Silences => "silences",
            Group::Rules => "alert-rules",
            Group::Channels => "alert-channels",
        };
        format!("kairos.{group}.{}/v1", self.sub().unwrap_or("help"))
    }
}

/// 解析 `kairos <group>` 之後的參數
pub fn parse_args(group: Group, args: impl Iterator<Item = OsString>) -> Result<Cmd, String> {
    let mut cmd = Cmd {
        group,
        args: Vec::new(),
        opts: Vec::new(),
        flags: Vec::new(),
        config_path: None,
        json: false,
        ascii: false,
        help: false,
    };
    let mut it = args.peekable();
    while let Some(a) = it.next() {
        if let Some(p) = take_config_arg(&a, &mut it)? {
            if cmd.config_path.replace(p).is_some() {
                return Err("--config 只能指定一次".into());
            }
            continue;
        }
        let a = a.into_string().map_err(|a| {
            format!(
                "無法辨識的參數（含有不是有效 Unicode 的字元）: {}",
                a.to_string_lossy()
            )
        })?;
        if a == "-h" || a == "--help" {
            cmd.help = true;
        } else if FLAGS.contains(&a.as_str()) {
            match a.as_str() {
                "--json" => cmd.json = true,
                "--ascii" => cmd.ascii = true,
                _ => cmd.flags.push(a),
            }
        } else if let Some(name) = a.strip_prefix("--") {
            let (name, value) = match name.split_once('=') {
                Some((n, v)) => (n.to_string(), v.to_string()),
                None => {
                    let v = it
                        .next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or_else(|| format!("--{name} 需要一個值"))?;
                    (name.to_string(), v)
                }
            };
            cmd.opts.push((format!("--{name}"), value));
        } else {
            cmd.args.push(a);
        }
    }
    if cmd.args.is_empty() && !cmd.help {
        return Err("需要子命令（--help 顯示說明）".into());
    }
    Ok(cmd)
}

pub fn usage(group: Group) -> &'static str {
    match group {
        Group::Alerts => ALERTS_USAGE,
        Group::Silences => SILENCES_USAGE,
        Group::Rules => RULES_USAGE,
        Group::Channels => CHANNELS_USAGE,
    }
}

fn usage_err(msg: impl Into<String>) -> Failure {
    Failure::new(
        "用法錯誤",
        msg,
        vec!["以 --help 查看用法。".into()],
        EXIT_USAGE,
    )
}

/// 執行環境（測試可替換）
pub struct Env<'a> {
    pub now: DateTime<Utc>,
    pub tz: FixedOffset,
    pub unicode: bool,
    pub confirm: &'a mut dyn FnMut(&str) -> Option<String>,
    /// 等待執行中的 Kairos 處理動作的上限
    pub wait: Duration,
}

fn ok_json(cmd: &Cmd, now: DateTime<Utc>, data: Value) -> Output {
    Output {
        stdout: ascii_json(&json!({
            "schema": cmd.schema(), "ok": true, "generated_at": fmt_time(now),
            "data": data, "warnings": [], "error": null,
        })) + "\n",
        ..Default::default()
    }
}

fn db(cmd: &Cmd) -> Result<rusqlite::Connection, Failure> {
    match open(cmd.config_path.as_deref(), true)? {
        Some(c) => Ok(c),
        None => Err(db_failure("資料庫不存在")),
    }
}

/// 執行命令（不直接寫 stdout／stderr）
pub fn execute(cmd: &Cmd, env: &mut Env<'_>) -> Output {
    if cmd.help {
        return Output {
            stdout: format!("{}\n", usage(cmd.group)),
            ..Default::default()
        };
    }
    let result = match cmd.group {
        Group::Alerts => alerts_cmd(cmd, env),
        Group::Silences => silences_cmd(cmd, env),
        Group::Rules => rules_cmd(cmd, env),
        Group::Channels => channels_cmd(cmd, env),
    };
    match result {
        Ok(out) => out,
        Err(f) => Output {
            stdout: if cmd.json {
                f.json(&cmd.schema(), env.now)
            } else {
                String::new()
            },
            stderr: if cmd.json {
                String::new()
            } else {
                f.human(env.unicode)
            },
            exit: f.exit,
        },
    }
}

fn mark(env: &Env<'_>) -> &'static str {
    if env.unicode { "✓" } else { "[ OK ]" }
}

fn severity_cell(s: Severity, unicode: bool) -> String {
    let sym = match (s, unicode) {
        (Severity::Emergency | Severity::Critical, true) => "✗",
        (Severity::Emergency | Severity::Critical, false) => "[FAIL]",
        (Severity::Warning, true) => "⚠",
        (Severity::Warning, false) => "[WARN]",
        (Severity::Info, true) => "i",
        (Severity::Info, false) => "[INFO]",
    };
    format!("{sym} {}", s.label())
}

/// 時間：RFC 3339，或本機時區的 `2026-10-20T08:00`／`2026-10-20 08:00`
fn parse_time(text: &str, tz: FixedOffset) -> Option<i64> {
    if let Ok(t) = DateTime::parse_from_rfc3339(text.trim()) {
        return Some(t.timestamp_millis());
    }
    for f in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(text.trim(), f) {
            return n
                .and_local_timezone(tz)
                .single()
                .map(|t| t.timestamp_millis());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// alerts
// ---------------------------------------------------------------------------

fn alerts_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    match cmd.sub() {
        Some("list") => {
            let view = cmd.opt("--view").unwrap_or("active").to_string();
            if !matches!(view.as_str(), "active" | "needs_me" | "recovered" | "all") {
                return Err(usage_err(
                    "--view 必須是 active、needs_me、recovered 或 all",
                ));
            }
            let severity = cmd.opts_all("--severity");
            if let Some(b) = severity.iter().find(|s| Severity::parse(s).is_none()) {
                return Err(usage_err(format!(
                    "--severity 的值 \"{b}\" 無效（emergency、critical、warning、info）"
                )));
            }
            let limit = match cmd.opt("--limit") {
                Some(v) => v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=1000).contains(n))
                    .ok_or_else(|| usage_err("--limit 必須是 1～1000"))?,
                None => 100,
            };
            let conn = db(cmd)?;
            let filter = store::EventFilter {
                view,
                severity,
                limit,
                ..Default::default()
            };
            let (events, total) = store::list_events(&conn, &filter).map_err(db_failure)?;
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!({
                        "events": events.iter().map(ops::event_json).collect::<Vec<_>>(),
                        "total": total,
                    }),
                ));
            }
            if events.is_empty() {
                return Ok(Output {
                    stdout: "沒有符合條件的事件。\n".into(),
                    ..Default::default()
                });
            }
            let mut rows = vec![vec![
                "嚴重度".into(),
                "狀態".into(),
                "事件編號".into(),
                "事件".into(),
                format!("開始（{}）", zone_label(env.tz)),
                "持續".into(),
                "負責人".into(),
            ]];
            for e in &events {
                let state = match (e.state, e.silenced) {
                    (s, true) => format!("{}（已靜音）", s.label()),
                    (s, false) => s.label().to_string(),
                };
                let end = e.resolved_at.or(e.recovered_at).unwrap_or(now_ms);
                rows.push(vec![
                    severity_cell(e.severity, env.unicode),
                    state,
                    e.event_id.clone(),
                    crate::hosts_cli::escape_text(&e.title),
                    human_time(e.started_at, env.tz),
                    crate::alerts::notify::render::duration(end - e.started_at),
                    e.assignee.clone().unwrap_or_else(|| "—".into()),
                ]);
            }
            let mut out = table(&rows);
            out.push_str(&format!("\n共 {total} 個（顯示 {}）\n", events.len()));
            Ok(Output {
                stdout: out,
                ..Default::default()
            })
        }
        Some("show") => {
            let [_, id] = cmd.args.as_slice() else {
                return Err(usage_err("kairos alerts show 需要一個事件編號"));
            };
            let conn = db(cmd)?;
            let ev = store::get_event(&conn, id)
                .map_err(db_failure)?
                .ok_or_else(|| {
                    Failure::new(
                        "找不到事件",
                        format!("沒有事件 {id}（可能已依保留期限清除）"),
                        Vec::new(),
                        EXIT_GENERAL,
                    )
                })?;
            let timeline = store::timeline(&conn, id, 200).map_err(db_failure)?;
            let (children, total) = store::children(&conn, id, 20).map_err(db_failure)?;
            if cmd.json {
                let mut data = ops::event_json(&ev);
                data["timeline"] = json!(timeline);
                data["children"] = json!({ "total": total, "items": children.iter().map(ops::event_json).collect::<Vec<_>>() });
                return Ok(ok_json(cmd, env.now, data));
            }
            let mut out = format!(
                "{}  {}  {}\n{}\n",
                severity_cell(ev.severity, env.unicode),
                ev.state.label(),
                ev.event_id,
                crate::hosts_cli::escape_text(&ev.title)
            );
            out.push_str(&format!(
                "  開始    {} {}\n",
                human_time(ev.started_at, env.tz),
                zone_label(env.tz)
            ));
            if let Some(c) = &ev.code {
                out.push_str(&format!("  代碼    {c}\n"));
            }
            if let Some(a) = &ev.acked_by {
                out.push_str(&format!(
                    "  確認    {}（{}）\n",
                    crate::hosts_cli::escape_text(a),
                    ev.acked_at.map(|t| relative(now_ms, t)).unwrap_or_default()
                ));
            }
            if total > 0 {
                out.push_str(&format!("  子事件  {total} 個\n"));
            }
            if let Some(r) = ev.root_cause {
                out.push_str(&format!("  根因    {}\n", r.label()));
            }
            out.push_str("處理紀錄：\n");
            for t in &timeline {
                out.push_str(&format!(
                    "  {}  {}  {}\n",
                    t["at"]
                        .as_str()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| human_time(d.timestamp_millis(), env.tz))
                        .unwrap_or_default(),
                    t["actor"].as_str().unwrap_or(""),
                    t["action"].as_str().unwrap_or("")
                ));
            }
            Ok(Output {
                stdout: out,
                ..Default::default()
            })
        }
        Some(a @ ("ack" | "resolve" | "comment")) => {
            let ids: Vec<String> = cmd.args[1..].to_vec();
            if ids.is_empty() || (a != "ack" && ids.len() != 1) {
                return Err(usage_err(format!(
                    "kairos alerts {a} 需要{}事件編號",
                    if a == "ack" { "至少一個" } else { "一個" }
                )));
            }
            let params = match a {
                "ack" => {
                    let eta = cmd
                        .opt("--eta")
                        .map(|v| {
                            v.parse::<u64>()
                                .ok()
                                .filter(|m| (1..=1440).contains(m))
                                .ok_or_else(|| usage_err("--eta 必須是 1～1440 分鐘"))
                        })
                        .transpose()?;
                    json!({ "assign_to_me": cmd.flag("--assign-to-me"), "eta_minutes": eta })
                }
                "resolve" => {
                    let cause = cmd.opt("--root-cause").ok_or_else(|| usage_err("--root-cause 必填（hardware、network、unregistered_maintenance、software、false_positive、other）"))?;
                    if crate::alerts::model::RootCause::parse_user(cause).is_none() {
                        return Err(usage_err(format!("--root-cause 的值 \"{cause}\" 無效")));
                    }
                    json!({ "root_cause": cause, "note": cmd.opt("--note") })
                }
                _ => json!({ "text": cmd.opt("--text").ok_or_else(|| usage_err("--text 必填"))? }),
            };
            let conn = db(cmd)?;
            let actor = operator();
            let mut queued = Vec::new();
            for id in &ids {
                let qid = store::enqueue_action(&conn, now_ms, &actor, id, a, &params)
                    .map_err(db_failure)?;
                queued.push((id.clone(), qid));
            }
            // 等執行中的 Kairos 處理
            let deadline = std::time::Instant::now() + env.wait;
            let mut results: Vec<(String, Option<Value>)> = Vec::new();
            loop {
                results.clear();
                for (id, qid) in &queued {
                    results.push((
                        id.clone(),
                        store::action_result(&conn, *qid).map_err(db_failure)?,
                    ));
                }
                if results.iter().all(|(_, r)| r.is_some()) || std::time::Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let pending = results.iter().filter(|(_, r)| r.is_none()).count();
            let failed: Vec<&(String, Option<Value>)> = results
                .iter()
                .filter(|(_, r)| r.as_ref().is_some_and(|v| v["ok"] != json!(true)))
                .collect();
            let exit = if !failed.is_empty() {
                EXIT_REJECTED
            } else if pending > 0 {
                EXIT_WAITING
            } else {
                0
            };
            if cmd.json {
                let mut out = ok_json(
                    cmd,
                    env.now,
                    json!({
                        "results": results.iter().map(|(id, r)| json!({ "event_id": id, "processed": r.is_some(), "result": r })).collect::<Vec<_>>(),
                    }),
                );
                out.exit = exit;
                return Ok(out);
            }
            let mut stderr = String::new();
            let verb = match a {
                "ack" => "已確認",
                "resolve" => "已解決",
                _ => "已新增註解：",
            };
            for (id, r) in &results {
                match r {
                    Some(v) if v["ok"] == json!(true) => stderr.push_str(&format!("{} {verb} {id}\n", mark(env))),
                    Some(v) => stderr.push_str(&format!(
                        "{} 錯誤：{id}：{}\n",
                        if env.unicode { "✗" } else { "[FAIL]" },
                        v["message"].as_str().unwrap_or("")
                    )),
                    None => stderr.push_str(&format!(
                        "{} {id} 已排入佇列：執行中的 Kairos 在 2 秒內處理；Kairos 沒有執行時在下次啟動後處理\n",
                        if env.unicode { "…" } else { "[WAIT]" }
                    )),
                }
            }
            Ok(Output {
                stderr,
                exit,
                ..Default::default()
            })
        }
        Some(other) => Err(usage_err(format!(
            "無法辨識的子命令: {other}（list、show、ack、resolve、comment）"
        ))),
        None => Err(usage_err("需要子命令")),
    }
}

// ---------------------------------------------------------------------------
// silences
// ---------------------------------------------------------------------------

/// 清冊中使用中的主機（範圍的台數）
fn host_facts(conn: &rusqlite::Connection) -> Result<Vec<HostFacts>, Failure> {
    let mut stmt = conn
        .prepare("SELECT agent_id, tags, peer FROM hosts WHERE lifecycle = 'active'")
        .map_err(db_failure)?;
    let rows = stmt
        .query_map([], |r| {
            let tags: Option<String> = r.get(1)?;
            let peer: Option<String> = r.get(2)?;
            Ok(HostFacts {
                agent_id: r.get(0)?,
                tags: tags
                    .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.to_lowercase())
                    .collect(),
                ip: peer.as_deref().and_then(crate::bus::peer_ip),
            })
        })
        .map_err(db_failure)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_failure)
}

fn count_matching(hosts: &[HostFacts], s: &Scope) -> usize {
    let c = Compiled::new(s);
    hosts.iter().filter(|h| c.matches(h)).count()
}

fn silences_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    match cmd.sub() {
        Some("list") => {
            let status = cmd.opt("--status").unwrap_or("all").to_string();
            if !matches!(status.as_str(), "active" | "upcoming" | "ended" | "all") {
                return Err(usage_err("--status 必須是 active、upcoming、ended 或 all"));
            }
            let conn = db(cmd)?;
            let list = store::list_silences(&conn, &status, now_ms, 1000).map_err(db_failure)?;
            let hosts = host_facts(&conn)?;
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!(
                        list.iter()
                            .map(|s| ops::silence_json(
                                s,
                                now_ms,
                                Some(count_matching(&hosts, &s.scope))
                            ))
                            .collect::<Vec<_>>()
                    ),
                ));
            }
            if list.is_empty() {
                return Ok(Output {
                    stdout: "目前沒有靜音或維護時段。\n".into(),
                    ..Default::default()
                });
            }
            let mut rows = vec![vec![
                "SILENCE_ID".into(),
                "類型".into(),
                "狀態".into(),
                "名稱".into(),
                "範圍".into(),
                format!("時間（{}）", zone_label(env.tz)),
                "建立者".into(),
            ]];
            for s in &list {
                let status = match s.status(now_ms) {
                    "active" => "進行中",
                    "upcoming" => "即將開始",
                    _ => "已結束",
                };
                rows.push(vec![
                    s.silence_id.clone(),
                    if s.kind == SilenceKind::Maintenance {
                        "維護".into()
                    } else {
                        "靜音".into()
                    },
                    status.into(),
                    crate::hosts_cli::escape_text(&s.name),
                    format!(
                        "{}（{} 台）",
                        scope::short(&s.scope),
                        count_matching(&hosts, &s.scope)
                    ),
                    format!(
                        "{}–{}",
                        human_time(s.starts_at, env.tz),
                        human_time(s.effective_end(), env.tz)
                    ),
                    crate::hosts_cli::escape_text(&s.created_by),
                ]);
            }
            Ok(Output {
                stdout: table(&rows),
                ..Default::default()
            })
        }
        Some("create") => {
            let start = cmd
                .opt("--start")
                .map(|v| {
                    parse_time(v, env.tz)
                        .ok_or_else(|| usage_err(format!("--start 的值 \"{v}\" 無效")))
                })
                .transpose()?;
            let until = cmd
                .opt("--until")
                .map(|v| {
                    parse_time(v, env.tz)
                        .ok_or_else(|| usage_err(format!("--until 的值 \"{v}\" 無效")))
                })
                .transpose()?;
            let duration = cmd
                .opt("--duration")
                .map(|v| {
                    parse_duration(v)
                        .filter(|d| !d.is_zero())
                        .map(|d| d.as_secs())
                        .ok_or_else(|| {
                            usage_err(format!("--duration 的值 \"{v}\" 無效，例如 30m、4h、2d"))
                        })
                })
                .transpose()?;
            let input = SilenceInput {
                kind: Some(
                    if cmd.flag("--maintenance") {
                        "maintenance"
                    } else {
                        "silence"
                    }
                    .into(),
                ),
                name: cmd.opt("--name").map(str::to_string),
                scope: Some(Scope {
                    tags: cmd.opts_all("--tag"),
                    subnets: cmd.opts_all("--subnet"),
                    hosts: cmd.opts_all("--host"),
                }),
                rules: Some(cmd.opts_all("--rule")),
                starts_at: start,
                ends_at: until,
                duration_secs: duration,
                reason: cmd.opt("--reason").map(str::to_string),
                notify_on_end: Some(!cmd.flag("--no-end-notify")),
                exclude_uptime: None,
                from_event: cmd.opt("--from-event").map(str::to_string),
            };
            let conn = db(cmd)?;
            let rules = store::load_rules(&conn).map_err(db_failure)?;
            let from = match &input.from_event {
                Some(id) => Some(
                    store::get_event(&conn, id)
                        .map_err(db_failure)?
                        .ok_or_else(|| usage_err(format!("沒有事件 {id}")))?,
                ),
                None => None,
            };
            let hosts = host_facts(&conn)?;
            let scope_n = scope::normalize(input.scope.as_ref().unwrap_or(&Scope::default()))
                .map_err(|(_, m)| usage_err(m))?;
            let matching = count_matching(&hosts, &scope_n);
            let (silence, plan) = ops::validate_silence(
                &input,
                None,
                &rules,
                from.as_ref(),
                matching,
                &operator(),
                now_ms,
            )
            .map_err(|e| {
                Failure::new(
                    "無法建立靜音",
                    e.message,
                    vec!["縮短時長，或改用維護時段（--maintenance）。".into()],
                    EXIT_USAGE,
                )
            })?;
            if !plan.confirm.is_empty() && !cmd.flag("--yes") {
                let prompt = format!(
                    "{}\n  {}\n  目前符合 {matching} 台主機\n是否繼續？[y/N] ",
                    plan.confirm.join("\n"),
                    scope::sentence(&silence.scope)
                );
                match (env.confirm)(&prompt) {
                    Some(a) if a.trim().eq_ignore_ascii_case("y") => {}
                    Some(_) => {
                        return Err(Failure::new(
                            "已取消",
                            "沒有建立靜音",
                            Vec::new(),
                            EXIT_GENERAL,
                        ));
                    }
                    None => {
                        return Err(Failure::new(
                            "需要確認",
                            plan.confirm.join("；"),
                            vec!["確認範圍無誤後加上 --yes 再執行。".into()],
                            EXIT_USAGE,
                        ));
                    }
                }
            }
            store::save_silence(&conn, &silence).map_err(db_failure)?;
            let _ = crate::enroll::store::audit(
                &conn,
                now_ms,
                &crate::enroll::store::Audit {
                    actor: &operator(),
                    actor_ip: None,
                    category: "alerts",
                    action: "silence.create",
                    object: Some(&silence.silence_id),
                    result: "ok",
                    code: None,
                    details: Some(
                        json!({ "name": silence.name, "scope": scope::sentence(&silence.scope), "rules": silence.rules, "reason": silence.reason }),
                    ),
                },
            );
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    ops::silence_json(&silence, now_ms, Some(matching)),
                ));
            }
            Ok(Output {
                stdout: format!("{}\n", silence.silence_id),
                stderr: format!(
                    "{} 已建立{}「{}」（{}，目前符合 {matching} 台主機；{} 至 {} {}）\n",
                    mark(env),
                    if silence.kind == SilenceKind::Maintenance {
                        "維護時段"
                    } else {
                        "臨時靜音"
                    },
                    crate::hosts_cli::escape_text(&silence.name),
                    scope::sentence(&silence.scope),
                    human_time(silence.starts_at, env.tz),
                    human_time(silence.ends_at, env.tz),
                    zone_label(env.tz)
                ),
                ..Default::default()
            })
        }
        Some("end") => {
            let [_, id] = cmd.args.as_slice() else {
                return Err(usage_err("kairos silences end 需要一個 silence_id"));
            };
            let conn = db(cmd)?;
            let mut s = store::get_silence(&conn, id)
                .map_err(db_failure)?
                .ok_or_else(|| {
                    Failure::new(
                        "找不到靜音",
                        format!("沒有 {id}"),
                        vec!["kairos silences list 可查。".into()],
                        EXIT_GENERAL,
                    )
                })?;
            if s.status(now_ms) != "ended" {
                let by = operator();
                s.ended_at = Some(now_ms);
                s.ended_by = Some(by.clone());
                s.end_reason = cmd.opt("--reason").map(str::to_string);
                s.updated_at = now_ms;
                s.updated_by = by.clone();
                store::save_silence(&conn, &s).map_err(db_failure)?;
                let _ = crate::enroll::store::audit(
                    &conn,
                    now_ms,
                    &crate::enroll::store::Audit {
                        actor: &by,
                        actor_ip: None,
                        category: "alerts",
                        action: "silence.end",
                        object: Some(id),
                        result: "ok",
                        code: None,
                        details: Some(json!({ "reason": s.end_reason })),
                    },
                );
            }
            if cmd.json {
                return Ok(ok_json(cmd, env.now, ops::silence_json(&s, now_ms, None)));
            }
            Ok(Output {
                stderr: format!(
                    "{} 已提前結束「{}」\n",
                    mark(env),
                    crate::hosts_cli::escape_text(&s.name)
                ),
                ..Default::default()
            })
        }
        Some(other) => Err(usage_err(format!(
            "無法辨識的子命令: {other}（list、create、end）"
        ))),
        None => Err(usage_err("需要子命令")),
    }
}

// ---------------------------------------------------------------------------
// alert-rules、alert-channels
// ---------------------------------------------------------------------------

fn rules_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    match cmd.sub() {
        Some("list") => {
            let conn = db(cmd)?;
            let rules = store::load_rules(&conn).map_err(db_failure)?;
            let stats = store::rule_stats(&conn, now_ms - 7 * 86_400_000).map_err(db_failure)?;
            if cmd.json {
                return Ok(ok_json(cmd, env.now, json!(rules.iter().map(|r| json!({
                    "rule_id": r.rule_id, "builtin": r.builtin, "kind": r.kind, "category": r.category,
                    "name": r.name, "enabled": r.enabled, "severity": r.severity, "max_severity": r.max_severity(),
                    "channels": r.channels, "fires_7d": stats.get(&r.rule_id).map_or(0, |s| s.0),
                })).collect::<Vec<_>>())));
            }
            let mut rows = vec![vec![
                "啟用".into(),
                "RULE_ID".into(),
                "規則名稱".into(),
                "類型".into(),
                "嚴重度".into(),
                "通知管道".into(),
                "7 天觸發".into(),
            ]];
            for r in &rules {
                let sev = if r.max_severity() != r.severity {
                    format!("{} → {}", r.severity.label(), r.max_severity().label())
                } else {
                    r.severity.label().to_string()
                };
                rows.push(vec![
                    if r.enabled {
                        "●".into()
                    } else {
                        "○".into()
                    },
                    r.rule_id.clone(),
                    format!(
                        "{}{}",
                        crate::hosts_cli::escape_text(&r.name),
                        if r.builtin { "（系統預設）" } else { "" }
                    ),
                    r.category.as_str().into(),
                    sev,
                    r.channels.len().to_string(),
                    stats.get(&r.rule_id).map_or(0, |s| s.0).to_string(),
                ]);
            }
            Ok(Output {
                stdout: table(&rows),
                ..Default::default()
            })
        }
        Some(a @ ("enable" | "disable")) => {
            let [_, id] = cmd.args.as_slice() else {
                return Err(usage_err(format!(
                    "kairos alert-rules {a} 需要一個 rule_id"
                )));
            };
            let conn = db(cmd)?;
            let mut r = store::get_rule(&conn, id)
                .map_err(db_failure)?
                .ok_or_else(|| {
                    Failure::new(
                        "找不到規則",
                        format!("沒有規則 {id}"),
                        vec!["kairos alert-rules list 可查。".into()],
                        EXIT_GENERAL,
                    )
                })?;
            let on = a == "enable";
            let reason = cmd.opt("--reason").map(str::to_string);
            if !on && r.builtin {
                if reason.as_deref().is_none_or(|x| x.trim().is_empty()) {
                    return Err(usage_err("停用系統預設規則必須以 --reason 填寫原因"));
                }
                if !cmd.flag("--yes") {
                    match (env.confirm)(&format!(
                        "即將停用系統預設規則「{}」，停用期間不會產生這類事件。\n是否繼續？[y/N] ",
                        r.name
                    )) {
                        Some(x) if x.trim().eq_ignore_ascii_case("y") => {}
                        Some(_) => {
                            return Err(Failure::new(
                                "已取消",
                                "規則維持啟用",
                                Vec::new(),
                                EXIT_GENERAL,
                            ));
                        }
                        None => return Err(usage_err("非互動執行時停用系統預設規則需要 --yes")),
                    }
                }
            }
            if r.enabled != on {
                r.enabled = on;
                r.disabled_reason = if on { None } else { reason.clone() };
                r.updated_at = now_ms;
                r.updated_by = operator();
                store::save_rule(&conn, &r).map_err(db_failure)?;
                let _ = crate::enroll::store::audit(
                    &conn,
                    now_ms,
                    &crate::enroll::store::Audit {
                        actor: &operator(),
                        actor_ip: None,
                        category: "alerts",
                        action: if on {
                            "alert_rule.enable"
                        } else {
                            "alert_rule.disable"
                        },
                        object: Some(id),
                        result: "ok",
                        code: None,
                        details: Some(json!({ "reason": reason })),
                    },
                );
            }
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!({ "rule_id": r.rule_id, "enabled": r.enabled }),
                ));
            }
            Ok(Output {
                stderr: format!(
                    "{} 已{}規則「{}」（執行中的 Kairos 在 5 秒內生效）\n",
                    mark(env),
                    if on { "啟用" } else { "停用" },
                    r.name
                ),
                ..Default::default()
            })
        }
        Some(other) => Err(usage_err(format!(
            "無法辨識的子命令: {other}（list、enable、disable）"
        ))),
        None => Err(usage_err("需要子命令")),
    }
}

fn channels_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    match cmd.sub() {
        Some("list") => {
            let conn = db(cmd)?;
            // 只顯示遮蔽值：不解密機密
            let list = store::load_channels(&conn, None).map_err(db_failure)?;
            if cmd.json {
                return Ok(ok_json(cmd, env.now, json!(list.iter().map(|c| json!({
                    "channel_id": c.channel_id, "name": c.name, "kind": c.kind, "enabled": c.enabled,
                    "state": c.state, "config": c.config, "last_error_code": c.last_error_code,
                    "last_success_at": c.last_success_at.map(crate::api::http::ts_ms),
                })).collect::<Vec<_>>())));
            }
            if list.is_empty() {
                return Ok(Output {
                    stdout: "還沒有通知管道。沒有管道時告警只會出現在主控台。\n".into(),
                    ..Default::default()
                });
            }
            let now_ms = env.now.timestamp_millis();
            let mut rows = vec![vec![
                "狀態".into(),
                "CHANNEL_ID".into(),
                "名稱".into(),
                "類型".into(),
                "最近成功".into(),
                "最後的錯誤".into(),
            ]];
            for c in &list {
                let state = match (c.enabled, c.state.as_str(), env.unicode) {
                    (false, _, _) => "停用".to_string(),
                    (true, "failed", true) => "✗ 失敗".into(),
                    (true, "failed", false) => "[FAIL] 失敗".into(),
                    (true, _, true) => "✓ 正常".into(),
                    (true, _, false) => "[ OK ] 正常".into(),
                };
                rows.push(vec![
                    state,
                    c.channel_id.clone(),
                    crate::hosts_cli::escape_text(&c.name),
                    format!(
                        "{}{}",
                        c.kind.as_str(),
                        c.config
                            .get("format")
                            .and_then(Value::as_str)
                            .map(|f| format!("（{f}）"))
                            .unwrap_or_default()
                    ),
                    c.last_success_at
                        .map_or_else(|| "—".into(), |t| relative(now_ms, t)),
                    c.last_error_code.clone().unwrap_or_else(|| "—".into()),
                ]);
            }
            Ok(Output {
                stdout: table(&rows),
                ..Default::default()
            })
        }
        Some(other) => Err(usage_err(format!("無法辨識的子命令: {other}（list）"))),
        None => Err(usage_err("需要子命令")),
    }
}

/// 執行命令並寫出結果
pub fn run(cmd: Cmd) -> u8 {
    let unicode = !cmd.json && use_unicode(cmd.ascii);
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut confirm = |prompt: &str| -> Option<String> {
        if !interactive {
            return None;
        }
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).ok()?;
        Some(line)
    };
    let mut env = Env {
        now: Utc::now(),
        tz: Local::now().offset().fix(),
        unicode,
        confirm: &mut confirm,
        wait: Duration::from_secs(10),
    };
    let out = execute(&cmd, &mut env);
    let mut stdout = std::io::stdout().lock();
    let wrote = stdout
        .write_all(out.stdout.as_bytes())
        .and_then(|_| stdout.flush());
    eprint!("{}", out.stderr);
    match wrote {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => out.exit.max(EXIT_GENERAL),
        _ => out.exit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn arguments_and_times() {
        let c = parse_args(
            Group::Silences,
            os(&[
                "create", "--name", "x", "--tag", "a", "--tag=b", "--yes", "--json",
            ]),
        )
        .unwrap();
        assert_eq!(c.opts_all("--tag"), ["a", "b"]);
        assert!(c.flag("--yes") && c.json);
        assert_eq!(c.schema(), "kairos.silences.create/v1");
        assert!(parse_args(Group::Alerts, os(&[])).is_err());
        let tz = FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(
            parse_time("2026-10-20T08:00", tz),
            parse_time("2026-10-20T00:00:00Z", tz)
        );
        assert!(parse_time("tomorrow", tz).is_none());
        assert_eq!(severity_cell(Severity::Emergency, false), "[FAIL] 緊急");
        assert_eq!(severity_cell(Severity::Warning, true), "⚠ 警告");
    }

    /// 以暫存的資料目錄執行命令
    fn run_in(dir: &std::path::Path, group: Group, args: &[&str]) -> Output {
        let mut all: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        all.push("--config".into());
        all.push(dir.join("kairos.toml").to_string_lossy().into_owned());
        let cmd = parse_args(group, all.into_iter().map(OsString::from)).unwrap();
        let mut no = |_: &str| -> Option<String> { None };
        let mut env = Env {
            now: Utc::now(),
            tz: FixedOffset::east_opt(8 * 3600).unwrap(),
            unicode: false,
            confirm: &mut no,
            wait: Duration::ZERO,
        };
        execute(&cmd, &mut env)
    }

    #[test]
    fn silences_rules_and_queued_actions_from_the_cli() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-alerts-cli-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("data");
        std::fs::write(
            dir.join("kairos.toml"),
            format!("[storage]\ndata_dir = {:?}\n", data.to_string_lossy()),
        )
        .unwrap();
        // 資料庫在第一次執行時建立；系統預設規則由執行中的 Kairos 建立，這裡先補上
        let out = run_in(&dir, Group::Rules, &["list"]);
        assert_eq!(out.exit, 0, "{}", out.stderr);
        let conn = crate::token_cli::open(Some(&dir.join("kairos.toml")), false)
            .unwrap()
            .unwrap();
        store::ensure_builtin_rules(&conn, 0).unwrap();
        // 超過上限：只選 CRL 即將到期（系統＋緊急）時最長 4 小時
        let out = run_in(
            &dir,
            Group::Silences,
            &[
                "create",
                "--name",
                "x",
                "--reason",
                "r",
                "--duration",
                "5h",
                "--rule",
                "sys.crl_expiring",
            ],
        );
        assert_eq!(out.exit, EXIT_USAGE);
        assert!(out.stderr.contains("4 小時"), "{}", out.stderr);
        // 包含緊急等級（不選規則＝所有規則）：非互動需要 --yes
        let out = run_in(
            &dir,
            Group::Silences,
            &["create", "--name", "x", "--reason", "r", "--duration", "2h"],
        );
        assert_eq!(out.exit, EXIT_USAGE);
        assert!(out.stderr.contains("--yes"), "{}", out.stderr);
        let out = run_in(
            &dir,
            Group::Silences,
            &[
                "create",
                "--name",
                "歲修",
                "--reason",
                "r",
                "--duration",
                "2h",
                "--yes",
                "--subnet",
                "10.0.0.0/8",
            ],
        );
        assert_eq!(out.exit, 0, "{}", out.stderr);
        let sid = out.stdout.trim().to_string();
        assert!(sid.starts_with("s-"));
        let out = run_in(&dir, Group::Silences, &["list", "--json"]);
        assert!(out.stdout.contains(&sid));
        assert_eq!(run_in(&dir, Group::Silences, &["end", &sid]).exit, 0);
        // 停用系統預設規則：需要原因與確認
        assert_eq!(
            run_in(&dir, Group::Rules, &["disable", "sys.report_stale"]).exit,
            EXIT_USAGE
        );
        assert_eq!(
            run_in(
                &dir,
                Group::Rules,
                &["disable", "sys.report_stale", "--reason", "x"]
            )
            .exit,
            EXIT_USAGE
        );
        assert_eq!(
            run_in(
                &dir,
                Group::Rules,
                &["disable", "sys.report_stale", "--reason", "x", "--yes"]
            )
            .exit,
            0
        );
        // 確認：Kairos 沒有執行 → 排入佇列（結束代碼 7）
        let out = run_in(&dir, Group::Alerts, &["ack", "E-20261014-0001"]);
        assert_eq!(out.exit, EXIT_WAITING, "{}", out.stderr);
        assert_eq!(store::pending_actions(&conn).unwrap().len(), 1);
        assert_eq!(
            run_in(
                &dir,
                Group::Alerts,
                &["resolve", "E-1", "--root-cause", "bad"]
            )
            .exit,
            EXIT_USAGE
        );
        // 管道清單：沒有管道時的說明
        let out = run_in(&dir, Group::Channels, &["list"]);
        assert!(out.stdout.contains("還沒有通知管道"));
        // 稽核紀錄
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM audit_events WHERE category = 'alerts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
