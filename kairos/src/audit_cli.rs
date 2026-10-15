//! `kairos audit list|verify|export`：稽核日誌（`kairos/API.md` 第 12.8 節；設計包 S18）。
//!
//! - `verify`：重新計算雜湊鏈與 HMAC，指出第一個斷點；驗證失敗（雜湊鏈斷裂、HMAC 不符、無法驗證 HMAC）時
//!   結束代碼 9（有嚴重問題）並以 `KRS-E6201` 說明（設計包 copywriting.md 範例 95）。
//! - `export`：JSONL（每行一筆，含 `prev_hash` 與 `hash`，最後一行為鏈的起訖值），可離線重新計算。
//! - 驗證與匯出本身也寫入稽核（執行者為作業系統帳號）。

use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use crate::audit::{self, AuditFilter, CODE_CHAIN_BROKEN, Event};
use crate::config::take_config_arg;
use crate::hosts_cli::{ascii_json, parse_duration, use_unicode};
use crate::storage::fmt_time;
use crate::token_cli::{Failure, db_failure, human_time, open, operator, table, zone_label};

/// `kairos audit --help`
pub const AUDIT_USAGE: &str = "kairos audit — 稽核日誌（雜湊鏈；kairos/API.md 第 12.8 節）

用法：
  kairos audit list [--actor <執行者>] [--category <類別>] [--action <動作>] [--target <對象>]
                    [--result ok|failed|denied|rejected] [--since <時間> | --from <時間> --to <時間>] [--limit <N>]
  kairos audit verify
  kairos audit export [--out <檔案>] [--since <時間> | --from <時間> --to <時間>]

選項：
  --since <時間>     最近多久（90m、24h、7d、2w）
  --from、--to       RFC 3339 時間（例如 2026-10-01T00:00:00Z）
  --limit <N>        list 最多幾筆（預設 50，上限 1000），新到舊
  --out <檔案>       export 寫入檔案（不覆寫既有檔案）；沒有時寫到 stdout
  --config <path>    Kairos 的設定檔
  --json             list、verify 輸出機器可讀的 JSON（純 ASCII）
  --ascii            不使用 Unicode 符號

類別：login、user、api_token、settings、enroll、pki、host、audit、access、system。
任何角色都不能修改或刪除稽核紀錄；保留期限由 audit.retention_days 決定（預設 1095 天）。

結束代碼：0 完成（verify：雜湊鏈完整）、1 資料庫錯誤、2 用法錯誤、3 設定錯誤、
9 稽核日誌完整性驗證失敗（雜湊鏈斷裂、HMAC 不符或無法驗證 HMAC；KRS-E6201）、10 權限不足、11 輸出檔已存在或資料庫使用中";

const EXIT_BROKEN: u8 = 9;
const EXIT_CONFLICT: u8 = 11;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditAction {
    Help,
    List { limit: usize },
    Verify,
    Export { out: Option<PathBuf> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditCommand {
    pub action: AuditAction,
    pub config_path: Option<PathBuf>,
    pub json: bool,
    pub ascii: bool,
    pub actor: Option<String>,
    pub category: Option<String>,
    pub act: Option<String>,
    pub target: Option<String>,
    pub result: Option<String>,
    pub since: Option<std::time::Duration>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<AuditCommand, String> {
    let mut args = args.peekable();
    let mut cmd = AuditCommand {
        action: AuditAction::Help,
        config_path: None,
        json: false,
        ascii: false,
        actor: None,
        category: None,
        act: None,
        target: None,
        result: None,
        since: None,
        from: None,
        to: None,
    };
    let sub = match args.next() {
        None => return Err("kairos audit 需要子命令（list、verify、export）".to_string()),
        Some(a) if a == "-h" || a == "--help" => return Ok(cmd),
        Some(a) => a.to_string_lossy().into_owned(),
    };
    let (mut limit, mut out) = (50usize, None);
    while let Some(arg) = args.next() {
        if let Some(path) = take_config_arg(&arg, &mut args)? {
            if cmd.config_path.replace(path).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        let a = arg
            .to_str()
            .ok_or_else(|| format!("無法辨識的參數: {}", arg.to_string_lossy()))?
            .to_string();
        let mut value = |flag: &str| -> Result<String, String> {
            match a.strip_prefix(&format!("{flag}=")) {
                Some(v) => Ok(v.to_string()),
                None => args
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .ok_or_else(|| format!("{flag} 需要指定值")),
            }
        };
        let time = |flag: &str, v: String| -> Result<DateTime<Utc>, String> {
            DateTime::parse_from_rfc3339(&v)
                .map(|t| t.with_timezone(&Utc))
                .map_err(|_| format!("{flag} 的值 \"{v}\" 不是 RFC 3339 時間"))
        };
        let list_or_export = sub == "list" || sub == "export";
        match a.split('=').next().unwrap_or("") {
            "-h" | "--help" => {
                cmd.action = AuditAction::Help;
                return Ok(cmd);
            }
            "--json" if sub != "export" => cmd.json = true,
            "--ascii" => cmd.ascii = true,
            "--actor" if sub == "list" => cmd.actor = Some(value("--actor")?),
            "--category" if sub == "list" => cmd.category = Some(value("--category")?),
            "--action" if sub == "list" => cmd.act = Some(value("--action")?),
            "--target" if sub == "list" => cmd.target = Some(value("--target")?),
            "--result" if sub == "list" => {
                let r = value("--result")?;
                if !matches!(r.as_str(), "ok" | "failed" | "denied" | "rejected") {
                    return Err(format!(
                        "--result 的值 \"{r}\" 無效（ok、failed、denied、rejected）"
                    ));
                }
                cmd.result = Some(r);
            }
            "--since" if list_or_export => {
                let v = value("--since")?;
                cmd.since = Some(
                    parse_duration(&v)
                        .ok_or_else(|| format!("--since 的值 \"{v}\" 無效，例如 24h、7d"))?,
                );
            }
            "--from" if list_or_export => cmd.from = Some(time("--from", value("--from")?)?),
            "--to" if list_or_export => cmd.to = Some(time("--to", value("--to")?)?),
            "--limit" if sub == "list" => {
                let v = value("--limit")?;
                limit = v
                    .parse()
                    .ok()
                    .filter(|n| (1..=1000).contains(n))
                    .ok_or_else(|| format!("--limit 必須介於 1～1000（目前為 {v}）"))?;
            }
            "--out" if sub == "export" => out = Some(PathBuf::from(value("--out")?)),
            _ => return Err(format!("無法辨識的參數: {a}")),
        }
    }
    if cmd.since.is_some() && (cmd.from.is_some() || cmd.to.is_some()) {
        return Err("--since 與 --from／--to 只能擇一".to_string());
    }
    cmd.action = match sub.as_str() {
        "list" => AuditAction::List { limit },
        "verify" => AuditAction::Verify,
        "export" => AuditAction::Export { out },
        _ => return Err(format!("無法辨識的子命令: {sub}（list、verify、export）")),
    };
    Ok(cmd)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit: u8,
}

fn filter_of(cmd: &AuditCommand, now: DateTime<Utc>) -> AuditFilter {
    AuditFilter {
        actor: cmd.actor.clone(),
        category: cmd.category.clone(),
        action: cmd.act.clone(),
        target: cmd.target.clone(),
        result: cmd.result.clone(),
        from_ms: cmd
            .since
            .map(|d| now.timestamp_millis() - i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .or(cmd.from.map(|t| t.timestamp_millis())),
        to_ms: cmd.to.map(|t| t.timestamp_millis()),
        before_id: None,
        after_id: None,
    }
}

pub fn execute(cmd: &AuditCommand, now: DateTime<Utc>, tz: FixedOffset, unicode: bool) -> Output {
    let schema = match cmd.action {
        AuditAction::Verify => "kairos.audit.verify/v1",
        AuditAction::Export { .. } => "kairos.audit.export/v1",
        _ => "kairos.audit.list/v1",
    };
    let fail = |f: Failure| Output {
        stdout: if cmd.json {
            f.json(schema, now)
        } else {
            String::new()
        },
        stderr: if cmd.json {
            String::new()
        } else {
            f.human(unicode)
        },
        exit: f.exit,
    };
    if cmd.action == AuditAction::Help {
        return Output {
            stdout: format!("{AUDIT_USAGE}\n"),
            ..Default::default()
        };
    }
    let conn = match open(cmd.config_path.as_deref(), false) {
        Ok(Some(c)) => c,
        Ok(None) => return fail(db_failure("資料庫不存在")),
        Err(f) => return fail(f),
    };
    let now_ms = now.timestamp_millis();
    let by = operator();
    let note =
        |action: &'static str, result: &'static str, code: Option<&'static str>, details: Value| {
            let _ = Event {
                actor: by.clone(),
                actor_ip: None,
                category: "audit",
                action,
                object: None,
                result,
                code,
                details: Some(details),
            }
            .append(&conn, now_ms);
        };
    let f = filter_of(cmd, now);
    match &cmd.action {
        AuditAction::Help => unreachable!(),
        AuditAction::List { limit } => {
            let (rows, total) = match audit::query(&conn, &f, *limit) {
                Ok(r) => r,
                Err(e) => return fail(db_failure(e)),
            };
            note(
                "audit.query",
                "ok",
                None,
                json!({ "matched": total, "via": "cli" }),
            );
            if cmd.json {
                return Output {
                    stdout: ascii_json(&json!({
                        "schema": schema, "ok": true, "generated_at": fmt_time(now),
                        "data": { "events": rows.iter().map(audit::AuditRow::json).collect::<Vec<_>>(), "total": total },
                        "warnings": [], "error": null,
                    })) + "\n",
                    ..Default::default()
                };
            }
            if rows.is_empty() {
                return Output {
                    stdout: "此期間沒有符合條件的紀錄。\n".to_string(),
                    ..Default::default()
                };
            }
            let zone = zone_label(tz);
            let mut t = vec![vec![
                "ID".to_string(),
                format!("時間（{zone}）"),
                "執行者".to_string(),
                "來源 IP".to_string(),
                "動作".to_string(),
                "對象".to_string(),
                "結果".to_string(),
            ]];
            for r in &rows {
                let clean = |s: &str| {
                    s.chars()
                        .map(|c| if c.is_control() { '?' } else { c })
                        .collect::<String>()
                };
                t.push(vec![
                    r.id.to_string(),
                    human_time(r.at, tz),
                    clean(&r.actor),
                    r.actor_ip.clone().unwrap_or_else(|| "—".into()),
                    r.action.clone(),
                    r.object.as_deref().map(clean).unwrap_or_else(|| "—".into()),
                    match r.result.as_str() {
                        "ok" => "成功",
                        "failed" => "失敗",
                        "denied" => "拒絕",
                        "rejected" => "拒絕",
                        x => x,
                    }
                    .to_string(),
                ]);
            }
            Output {
                stdout: format!(
                    "{}\n顯示 {} / {} 筆（新到舊）\n",
                    table(&t),
                    rows.len(),
                    total
                ),
                ..Default::default()
            }
        }
        AuditAction::Verify => {
            if let Err(e) = audit::seal_legacy(&conn) {
                return fail(db_failure(e));
            }
            let report = match audit::verify(&conn) {
                Ok(r) => r,
                Err(e) => return fail(db_failure(e)),
            };
            note(
                "audit.verify",
                if report.ok() { "ok" } else { "failed" },
                (!report.ok()).then_some(CODE_CHAIN_BROKEN),
                json!({ "checked": report.checked, "via": "cli" }),
            );
            let exit = if report.ok() { 0 } else { EXIT_BROKEN };
            if cmd.json {
                let mut issues = Vec::new();
                if let Some(b) = &report.broken {
                    issues.push(json!({
                        "code": CODE_CHAIN_BROKEN, "severity": "critical",
                        "title": "稽核日誌完整性驗證失敗",
                        "cause": format!("第 {} 筆：{}", b.id, b.reason),
                        "fix": ["保全資料庫檔（不要再寫入），與匯出或外送的副本比對，找出被修改或刪除的紀錄。"],
                        "details": { "id": b.id, "reason": b.reason }, "explain": null,
                    }));
                }
                let mut data = report.json();
                data["issues"] = json!(issues);
                return Output {
                    stdout: ascii_json(&json!({
                        "schema": schema, "ok": true, "generated_at": fmt_time(now),
                        "data": data, "warnings": [], "error": null,
                    })) + "\n",
                    exit,
                    ..Default::default()
                };
            }
            // HMAC 的邊界：之前的紀錄只有雜湊鏈（0.7 之前或服務第一次以 0.7 啟動前寫入）
            let boundary = match (report.hmac.since_id, report.first_id) {
                (None, _) => "HMAC：尚未開始（Kairos 服務以 0.7 啟動、建立 audit-hmac.key 之後寫入的第一筆起才有）".to_string(),
                (Some(s), Some(first)) if s > first => format!(
                    "HMAC：第 {s} 筆起（{} 筆）；第 {first}～{} 筆只有雜湊鏈（0.7 之前寫入）",
                    report.hmac.verified,
                    s - 1
                ),
                (Some(s), _) => format!("HMAC：第 {s} 筆起（{} 筆）", report.hmac.verified),
            };
            match &report.broken {
                None => Output {
                    stdout: format!(
                        "{} 稽核日誌完整：{} 筆，雜湊鏈連續（第 {}～{} 筆）\n  {boundary}\n  鏈的終點：{}\n",
                        if unicode { "✓" } else { "[ OK ]" },
                        report.checked,
                        report.first_id.map_or("—".into(), |x| x.to_string()),
                        report.last_id.map_or("—".into(), |x| x.to_string()),
                        report.last_hash.as_deref().unwrap_or("—"),
                    ),
                    ..Default::default()
                },
                Some(b) => {
                    let key_problem =
                        matches!(b.reason, "hmac_key_unavailable" | "hmac_key_mismatch");
                    Output {
                        stderr: format!(
                            "{} 錯誤 {CODE_CHAIN_BROKEN}：稽核日誌完整性驗證失敗\n  原因：第 {} 筆：{}。{}\n  {boundary}\n  處理方式：\n{}",
                            if unicode { "✗" } else { "[FAIL]" },
                            b.id,
                            crate::audit_monitor::describe(b.reason),
                            if key_problem {
                                "雜湊鏈本身完整，但無法確認紀錄沒有被整段重新計算。"
                            } else {
                                "紀錄可能被修改、刪除或插入。"
                            },
                            if key_problem {
                                "    1. 從備份還原資料目錄的 audit-hmac.key（kairos db restore-keys），再執行 kairos audit verify。\n    2. 金鑰確實遺失時，保全資料庫檔，並以外送的鏈尾雜湊（audit.chain_anchor）確認之前的紀錄。\n"
                            } else {
                                "    1. 保全資料庫檔（複製 kairos.db 與 -wal），不要再寫入。\n    2. 與先前匯出（kairos audit export）或外送的鏈尾雜湊（audit.chain_anchor）比對，找出差異。\n"
                            }
                        ),
                        exit,
                        ..Default::default()
                    }
                }
            }
        }
        AuditAction::Export { out } => {
            let mut text = String::new();
            let (mut count, mut first, mut last) = (
                0u64,
                None::<(i64, Option<String>)>,
                None::<(i64, Option<String>)>,
            );
            let mut after = 0i64;
            loop {
                let batch = AuditFilter {
                    after_id: Some(after),
                    ..f.clone()
                };
                let rows = match audit::export_batch(&conn, &batch, 5000) {
                    Ok(r) => r,
                    Err(e) => return fail(db_failure(e)),
                };
                if rows.is_empty() {
                    break;
                }
                for r in &rows {
                    if first.is_none() {
                        first = Some((r.id, r.prev_hash.clone()));
                    }
                    last = Some((r.id, r.hash.clone()));
                    count += 1;
                    text.push_str(&r.export_json().to_string());
                    text.push('\n');
                }
                after = rows.last().map_or(after, |r| r.id);
            }
            text.push_str(&json!({
                "type": "chain", "generated_at": fmt_time(now), "count": count,
                "first_id": first.as_ref().map(|x| x.0), "first_prev_hash": first.and_then(|x| x.1),
                "last_id": last.as_ref().map(|x| x.0), "last_hash": last.and_then(|x| x.1),
                "hmac_since": audit::hmac_since(&conn).ok().flatten().map(|s| s.0),
            }).to_string());
            text.push('\n');
            note(
                "audit.export",
                "ok",
                None,
                json!({ "count": count, "via": "cli", "out": out.as_ref().map(|p| p.display().to_string()) }),
            );
            match out {
                None => Output {
                    stdout: text,
                    ..Default::default()
                },
                Some(path) => {
                    let mut opts = std::fs::OpenOptions::new();
                    opts.write(true).create_new(true);
                    crate::fsperm::private_std_options(&mut opts);
                    match opts
                        .open(path)
                        .and_then(|mut f| f.write_all(text.as_bytes()))
                    {
                        Ok(()) => Output {
                            stderr: format!(
                                "{} 已匯出 {count} 筆稽核紀錄到 {}\n",
                                if unicode { "✓" } else { "[ OK ]" },
                                path.display()
                            ),
                            ..Default::default()
                        },
                        Err(e) if e.kind() == ErrorKind::AlreadyExists => fail(Failure::new(
                            "輸出檔已存在",
                            format!("{} 已存在，不覆寫", path.display()),
                            vec!["指定新的檔名，或先移走舊檔。".to_string()],
                            EXIT_CONFLICT,
                        )),
                        Err(e) => fail(Failure::new("無法寫入輸出檔", e.to_string(), vec![], 1)),
                    }
                }
            }
        }
    }
}

pub fn run(cmd: AuditCommand) -> u8 {
    let unicode = !cmd.json && use_unicode(cmd.ascii);
    let out = execute(&cmd, Utc::now(), Local::now().offset().fix(), unicode);
    let mut stdout = std::io::stdout().lock();
    let wrote = stdout
        .write_all(out.stdout.as_bytes())
        .and_then(|_| stdout.flush());
    eprint!("{}", out.stderr);
    match wrote {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => out.exit.max(1),
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
    fn argument_parsing() {
        let c = parse_args(os(&[
            "list",
            "--actor",
            "user:admin",
            "--since",
            "24h",
            "--limit",
            "10",
            "--json",
        ]))
        .unwrap();
        assert_eq!(c.action, AuditAction::List { limit: 10 });
        assert_eq!(c.actor.as_deref(), Some("user:admin"));
        assert!(
            parse_args(os(&[
                "list",
                "--since",
                "1h",
                "--from",
                "2026-01-01T00:00:00Z"
            ]))
            .is_err()
        );
        assert!(parse_args(os(&["list", "--result", "maybe"])).is_err());
        assert!(parse_args(os(&["export", "--json"])).is_err());
        assert!(parse_args(os(&["delete"])).is_err());
        assert_eq!(
            parse_args(os(&["verify"])).unwrap().action,
            AuditAction::Verify
        );
        assert!(!AUDIT_USAGE.contains("delete"));
    }

    #[test]
    fn verify_detects_tampering_and_export_is_verifiable() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-audit-cli-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("kairos.toml");
        std::fs::write(
            &cfg,
            format!(
                "[storage]\ndata_dir = \"{}\"\n",
                dir.join("data").display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        let conn = open(Some(&cfg), true).unwrap().unwrap();
        for i in 0..3 {
            audit::append(
                &conn,
                i,
                &audit::Audit {
                    actor: "system",
                    category: "system",
                    action: "system.start",
                    result: "ok",
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let tz = FixedOffset::east_opt(0).unwrap();
        let mut cmd = parse_args(os(&["verify", "--json"])).unwrap();
        cmd.config_path = Some(cfg.clone());
        let out = execute(&cmd, Utc::now(), tz, false);
        assert_eq!(out.exit, 0, "{out:?}");
        // 匯出：每一行可重新計算雜湊，最後一行是鏈的起訖值
        let mut ex = parse_args(os(&["export"])).unwrap();
        ex.config_path = Some(cfg.clone());
        let out = execute(&ex, Utc::now(), tz, false);
        let lines: Vec<Value> = out
            .stdout
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.last().unwrap()["type"], "chain");
        let mut prev = audit::GENESIS.to_string();
        for l in &lines[..lines.len() - 1] {
            assert_eq!(l["prev_hash"], prev.as_str());
            prev = l["hash"].as_str().unwrap().to_string();
        }
        assert_eq!(lines.last().unwrap()["last_hash"], prev.as_str());
        // 竄改第 2 筆：結束代碼 9、KRS-E6201
        conn.execute("UPDATE audit_events SET actor = 'mallory' WHERE id = 2", [])
            .unwrap();
        let out = execute(&cmd, Utc::now(), tz, false);
        assert_eq!(out.exit, EXIT_BROKEN);
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["data"]["broken"]["id"], 2);
        assert_eq!(v["data"]["issues"][0]["code"], "KRS-E6201");
        // 輸出檔已存在：不覆寫
        let path = dir.join("a.jsonl");
        std::fs::write(&path, "x").unwrap();
        ex.action = AuditAction::Export {
            out: Some(path.clone()),
        };
        assert_eq!(execute(&ex, Utc::now(), tz, false).exit, EXIT_CONFLICT);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
