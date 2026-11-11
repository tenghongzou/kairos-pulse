//! `kairos hosts purge|export|pseudonym`：個資的刪除、匯出與假名查詢（商業審計 S6；`kairos/PRIVACY.md`）。
//!
//! - `purge`：刪除一台主機的所有資料（L2）。`--dry-run` 只計算（唯讀，Kairos 執行中也可以）；實際刪除需要
//!   **Kairos 停止**（以獨占鎖開啟資料庫，與 `kairos reports import` 相同）。Kairos 執行中請改用主控台
//!   （S05「刪除主機資料」）或 `POST /api/v1/hosts/{agent_id}/purge`，由 Kairos 自己的 writer 刪除：
//!   另一個行程直接刪除時，執行中的 Kairos 記憶體中的快取、尚未寫出的彙總與狀態表會把資料寫回去。
//! - `export`：一台主機的個資（JSON，`kairos.host.personal-data/v1`；資料主體存取請求）。Kairos 執行中也可以。
//! - `pseudonym`：以 `data/privacy.key` 算出主機名稱（或顯示名稱、使用者）的假名，查詢「這台主機在
//!   Kairos 中叫什麼」（`privacy.hostname = "pseudonymize"` 時）。

use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::enroll_cli::{
    Cmd, EXIT_CONFLICT, EXIT_GENERAL, EXIT_USAGE, Env, load_config, mark, ok_json, usage_err,
};
use crate::store::db::DbHandle;
use crate::store::purge::{CODE_PURGE_FAILED, PurgeCounts, PurgeJob};
use crate::store::sqlite::{DB_FILE, SqliteOptions};
use crate::token_cli::{Failure, Output, db_failure, operator};

/// `kairos hosts purge|export|pseudonym --help`
pub const USAGE: &str = "kairos hosts purge|export|pseudonym — 個資的刪除、匯出與假名查詢（kairos/PRIVACY.md）

用法：
  kairos hosts purge <agent_id> --dry-run
  kairos hosts purge <agent_id> [--reason <原因>] [--yes --confirm <主機名稱>]
  kairos hosts export <agent_id> [--out <檔案>]
  kairos hosts pseudonym <名稱>... [--kind host|name|user]

purge 刪除這台主機的 report、彙總、資產、行程、告警事件、註冊申請與憑證紀錄，並撤銷尚未撤銷的憑證
（cessationOfOperation）。稽核日誌不刪除（雜湊鏈不可修改），另記一筆 host.purge。為了 CRL，已撤銷且
尚未過期的憑證紀錄保留到過期後 1 天。--dry-run 只列出會刪除的筆數（Kairos 執行中也可以）；實際刪除
需要先停止 Kairos（執行中請改用主控台的「刪除主機資料」或 POST /api/v1/hosts/<agent_id>/purge）。
L2：互動時輸入主機名稱確認；非互動需要 --yes 與 --confirm <主機名稱>。

export 輸出這台主機的個資（JSON；資料主體存取請求），未指定 --out 時寫到 stdout。
pseudonym 以 data/privacy.key 算出假名（privacy.hostname = \"pseudonymize\" 時，主控台顯示的名稱）。

通用選項：--config <path>、--json、--ascii、-h/--help
結束代碼：0 完成、1 找不到資料或資料庫錯誤、2 用法錯誤、3 設定錯誤、10 權限不足、11 資料庫使用中（Kairos 執行中）";

/// 執行 `kairos hosts purge|export|pseudonym`
pub fn execute(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    match cmd.sub(0) {
        Some("purge") => purge_cmd(cmd, env),
        Some("export") => export_cmd(cmd, env),
        Some("pseudonym") => pseudonym_cmd(cmd, env),
        _ => Err(usage_err("子命令必須是 purge、export 或 pseudonym")),
    }
}

fn agent_arg(cmd: &Cmd) -> Result<String, Failure> {
    let id = cmd
        .sub(1)
        .ok_or_else(|| usage_err("需要 agent_id（kairos hosts list --wide 可查詢）"))?;
    if !kairos_proto::is_uuid_text(id) {
        return Err(usage_err(format!("agent_id 必須是 UUID：{id}")));
    }
    if cmd.args.len() > 2 {
        return Err(usage_err("一次只能指定一台主機"));
    }
    Ok(id.to_ascii_lowercase())
}

fn not_found(agent_id: &str) -> Failure {
    Failure {
        code: Some(CODE_PURGE_FAILED),
        ..Failure::new(
            "找不到這台主機的資料",
            format!("資料庫中沒有 agent_id {agent_id} 的資料（可能已經刪除）。"),
            vec!["以 kairos hosts list --wide 確認 agent_id。".into()],
            EXIT_GENERAL,
        )
    }
}

fn data_dir(cmd: &Cmd) -> Result<(crate::config::AppConfig, PathBuf), Failure> {
    let config = load_config(cmd.config_path.as_deref())?;
    let dir = PathBuf::from(&config.storage.data_dir);
    Ok((config, dir))
}

fn open_read_only(dir: &Path) -> Result<DbHandle, Failure> {
    crate::store::open_read(&dir.join(DB_FILE)).map_err(db_failure)
}

fn counts_text(c: &PurgeCounts) -> String {
    let mut out = String::new();
    for (label, n) in c.rows() {
        out.push_str(&format!("  {label:<10}{n:>10}\n"));
    }
    out
}

fn purge_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let agent_id = agent_arg(cmd)?;
    let (config, dir) = data_dir(cmd)?;
    let now_ms = env.now.timestamp_millis();
    if cmd.flag("--dry-run") {
        let conn = open_read_only(&dir)?;
        let c = conn.purge_plan(&agent_id, now_ms).map_err(db_failure)?;
        if c.total() == 0 {
            return Err(not_found(&agent_id));
        }
        if cmd.json {
            return Ok(ok_json(
                cmd,
                env.now,
                json!({"agent_id": agent_id, "dry_run": true, "counts": c.json()}),
            ));
        }
        return Ok(Output {
            stdout: format!(
                "試執行：刪除 {agent_id} 會刪除以下資料（共 {} 列）：\n{}  會撤銷的憑證 {} 張；為了 CRL 保留到過期的憑證紀錄 {} 筆；稽核日誌不刪除。\n",
                c.total(),
                counts_text(&c),
                c.certificates_active,
                c.certificates_kept
            ),
            ..Default::default()
        });
    }
    if !crate::store::database_exists(&dir.join(DB_FILE)).unwrap_or(true) {
        return Err(not_found(&agent_id));
    }
    if let Some(f) = crate::token_cli::refuse_root(&dir) {
        return Err(f);
    }
    let opts = SqliteOptions::from_config(&config, &dir);
    let conn = crate::store::open_exclusive(&opts, Duration::from_secs(2)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::ResourceBusy {
            Failure {
                code: Some("KRS-E4304"),
                ..Failure::new(
                    "資料庫正在使用中",
                    "Kairos 執行中（或另一個 kairos 子命令正在使用資料庫）：刪除主機資料需要獨占資料庫。",
                    vec![
                        "Kairos 執行中請改用主控台的「刪除主機資料」（主機詳情 › 更多），或 POST /api/v1/hosts/<agent_id>/purge。".into(),
                        "或先停止 Kairos（systemctl stop kairos）再執行。".into(),
                    ],
                    EXIT_CONFLICT,
                )
            }
        } else {
            db_failure(e)
        }
    })?;
    let c = conn.purge_plan(&agent_id, now_ms).map_err(db_failure)?;
    if c.total() == 0 {
        return Err(not_found(&agent_id));
    }
    let hostname: Option<String> = conn.host_names(&agent_id).ok().flatten().map(|(h, _)| h);
    let want = hostname.clone().unwrap_or_else(|| agent_id.clone());
    if cmd.flag("--yes") {
        if cmd.opt("--confirm").map(str::trim) != Some(want.as_str()) {
            return Err(Failure::new(
                "未刪除",
                format!(
                    "非互動執行時需要 --yes 與 --confirm <主機名稱>（L2）；這台主機的名稱是「{want}」"
                ),
                Vec::new(),
                EXIT_USAGE,
            ));
        }
    } else {
        let typed = (env.confirm)(&format!(
            "即將刪除 {agent_id}（{want}）的所有資料（共 {} 列），並撤銷 {} 張有效的憑證；無法復原。稽核日誌不刪除。\n請輸入主機名稱「{want}」確認：",
            c.total(),
            c.certificates_active
        ));
        if typed.as_deref().map(str::trim) != Some(want.as_str()) {
            return Err(Failure::new(
                "未刪除",
                "確認字串不符（非互動執行時需要 --yes 與 --confirm <主機名稱>）",
                Vec::new(),
                EXIT_USAGE,
            ));
        }
    }
    let actor = operator();
    let revoked = conn
        .revoke_active_certificates(&agent_id, &actor, None, now_ms)
        .map_err(db_failure)?;
    let mut job = PurgeJob::new(&agent_id, &actor, None, cmd.opt("--reason"));
    job.revoked = revoked.clone();
    conn.purge_run_to_end(&mut job, now_ms)
        .map_err(|e| Failure {
            code: Some(CODE_PURGE_FAILED),
            ..Failure::new(
                "刪除沒有完成",
                format!("資料庫錯誤：{e}"),
                vec!["已刪除的部分不會復原；修正後重新執行同一個命令即可接續完成。".into()],
                EXIT_GENERAL,
            )
        })?;
    drop(conn);
    let d = &job.counts;
    if cmd.json {
        return Ok(ok_json(
            cmd,
            env.now,
            json!({"agent_id": agent_id, "deleted": d.json(), "revoked_serials": revoked}),
        ));
    }
    let mut text = format!(
        "{} 已刪除 {agent_id}（{want}）的資料（共 {} 列）：\n{}",
        mark(env),
        d.total(),
        counts_text(d)
    );
    if !revoked.is_empty() {
        text.push_str(&format!(
            "  已撤銷憑證：{}（Kairos 下次啟動時重新產生 CRL；離線簽發模式請匯出撤銷清單產生 CRL）\n",
            revoked.join("、")
        ));
    }
    text.push_str(&format!(
        "  為了 CRL 保留的已撤銷憑證紀錄 {} 筆（過期 1 天後自動刪除）；稽核日誌不刪除，已記錄 host.purge。\n  日誌檔、JSONL 鏡像與資料庫備份中的資料不受影響，依各自的保留期限刪除（kairos/PRIVACY.md 第 4 節）。\n",
        d.certificates_kept
    ));
    Ok(Output {
        stdout: text,
        ..Default::default()
    })
}

fn export_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let agent_id = agent_arg(cmd)?;
    let (config, _) = data_dir(cmd)?;
    let conn = match crate::token_cli::open(cmd.config_path.as_deref(), false)? {
        Some(c) => c,
        None => return Err(not_found(&agent_id)),
    };
    let now_ms = env.now.timestamp_millis();
    let doc = conn
        .personal_data(&agent_id, &config.privacy, now_ms)
        .map_err(db_failure)?
        .ok_or_else(|| not_found(&agent_id))?;
    let actor = operator();
    conn.audit_append(
        now_ms,
        &crate::audit::Audit {
            actor: &actor,
            category: "host",
            action: "host.personal_data.export",
            object: Some(&agent_id),
            result: "ok",
            details: cmd.opt("--out").map(|o| json!({"out": o})),
            ..crate::audit::Audit::default()
        },
    )
    .map_err(db_failure)?;
    let pretty = serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n";
    if let Some(out) = cmd.opt("--out") {
        crate::enroll_cli::write_new_file(out, pretty.as_bytes())?;
        if cmd.json {
            return Ok(ok_json(
                cmd,
                env.now,
                json!({"agent_id": agent_id, "out": out}),
            ));
        }
        return Ok(Output {
            stdout: format!(
                "{} 已將 {agent_id} 的個資寫到 {out}（不覆寫既有的檔案；內容含主機名稱等個資，請妥善保管）\n",
                mark(env)
            ),
            ..Default::default()
        });
    }
    if cmd.json {
        return Ok(ok_json(cmd, env.now, doc));
    }
    Ok(Output {
        stdout: pretty,
        ..Default::default()
    })
}

fn pseudonym_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let names = &cmd.args[1..];
    if names.is_empty() {
        return Err(usage_err("需要至少一個名稱"));
    }
    let kind = cmd.opt("--kind").unwrap_or("host");
    if !matches!(kind, "host" | "name" | "user") {
        return Err(usage_err(
            "--kind 必須是 host（主機名稱）、name（顯示名稱）或 user（行程的使用者）",
        ));
    }
    let (_, dir) = data_dir(cmd)?;
    let key = crate::privacy::PseudonymKey::load(&dir)
        .map_err(db_failure)?
        .ok_or_else(|| {
            Failure::new(
                "沒有假名化的金鑰",
                format!(
                    "{} 不存在：Kairos 還沒有以 privacy.hostname 或 privacy.process_user = \"pseudonymize\" 啟動過。",
                    dir.join(crate::privacy::KEY_FILE).display()
                ),
                vec!["設定 [privacy] 後重新啟動 Kairos（服務會建立金鑰）。".into()],
                EXIT_GENERAL,
            )
        })?;
    let pairs: Vec<(String, String)> = names
        .iter()
        .map(|n| (n.clone(), key.pseudonym(kind, n)))
        .collect();
    if cmd.json {
        return Ok(ok_json(
            cmd,
            env.now,
            json!({
                "kind": kind,
                "key_id": key.key_id(),
                "pseudonyms": pairs.iter().map(|(n, p)| json!({"name": n, "pseudonym": p})).collect::<Vec<_>>(),
            }),
        ));
    }
    let mut text = String::new();
    for (n, p) in pairs {
        text.push_str(&format!("{n}\t{p}\n"));
    }
    Ok(Output {
        stdout: text,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enroll_cli::{Group, parse_args};
    use chrono::{FixedOffset, TimeZone, Utc};
    use std::ffi::OsString;

    fn os(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn run(dir: &Path, args: &[&str], answer: Option<&str>) -> Output {
        let mut all: Vec<&str> = args.to_vec();
        let cfg = dir.join("kairos.toml");
        let cfg = cfg.to_str().unwrap().to_string();
        all.extend(["--config", &cfg]);
        let cmd = parse_args(Group::Hosts, os(&all)).unwrap();
        let mut confirm = |_: &str| answer.map(str::to_string);
        let mut env = Env {
            now: Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap(),
            tz: FixedOffset::east_opt(8 * 3600).unwrap(),
            unicode: false,
            confirm: &mut confirm,
        };
        crate::enroll_cli::execute(&cmd, &mut env)
    }

    const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";

    fn setup(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kairos-privacy-cli-{tag}-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(
            dir.join("kairos.toml"),
            format!(
                "[storage]\ndata_dir = \"{}\"\n",
                dir.join("data").to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        let config = crate::config::AppConfig::load(Some(&dir.join("kairos.toml"))).unwrap();
        let opts = SqliteOptions::from_config(&config, &dir.join("data"));
        let conn = crate::store::sqlite::open_admin(&opts, Duration::from_secs(1)).unwrap();
        conn.execute(
            "INSERT INTO hosts (agent_id, hostname, peer, first_seen, last_seen) VALUES (?1, 'ann-laptop', '10.0.0.9:1', 1, 2)",
            [A],
        )
        .unwrap();
        let host: i64 = conn
            .query_row("SELECT host_id FROM hosts", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO reports (host_id, ts, seq, format, body) VALUES (?1, 1, 1, 1, x'00')",
            [host],
        )
        .unwrap();
        dir
    }

    #[test]
    fn dry_run_then_l2_confirmation_then_purge() {
        let dir = setup("purge");
        let o = run(&dir, &["purge", A, "--dry-run"], None);
        assert_eq!(o.exit, 0, "{}", o.stderr);
        assert!(
            o.stdout.contains("原始 report") && o.stdout.contains("試執行"),
            "{}",
            o.stdout
        );
        // 確認字串不符：不刪除
        let o = run(&dir, &["purge", A], Some("wrong"));
        assert_eq!(o.exit, EXIT_USAGE, "{}", o.stderr);
        let o = run(&dir, &["purge", A, "--yes"], None);
        assert_eq!(o.exit, EXIT_USAGE);
        assert!(o.stderr.contains("ann-laptop"), "{}", o.stderr);
        // --json 的試執行
        let o = run(&dir, &["purge", A, "--dry-run", "--json"], None);
        let v: serde_json::Value = serde_json::from_str(o.stdout.trim()).unwrap();
        assert_eq!(v["schema"], "kairos.hosts.purge/v1");
        assert_eq!(v["data"]["counts"]["reports"], 1);
        // 個資匯出（刪除前）
        let o = run(&dir, &["export", A], None);
        assert_eq!(o.exit, 0, "{}", o.stderr);
        let doc: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
        assert_eq!(doc["schema"], crate::privacy::EXPORT_SCHEMA);
        assert_eq!(doc["host"]["hostname"], "ann-laptop");
        assert_eq!(doc["reports"]["count"], 1);
        assert_eq!(
            doc["host"]["first_seen"], 1,
            "小於 1973 年的值不是毫秒時間，不轉換"
        );
        // 互動確認後刪除
        let o = run(
            &dir,
            &["purge", A, "--reason", "員工離職"],
            Some("ann-laptop\n"),
        );
        assert_eq!(o.exit, 0, "{}", o.stderr);
        assert!(o.stdout.contains("已刪除"), "{}", o.stdout);
        let o = run(&dir, &["purge", A, "--dry-run"], None);
        assert_eq!(o.exit, EXIT_GENERAL);
        assert!(o.stderr.contains(CODE_PURGE_FAILED), "{}", o.stderr);
        // 匯出：已沒有資料；稽核中有 host.purge 與兩次匯出
        let o = run(&dir, &["export", A], None);
        assert_eq!(o.exit, EXIT_GENERAL);
        let conn = open_read_only(&dir.join("data")).unwrap();
        let n: i64 = conn
            .test_sqlite().query_row(
                "SELECT COUNT(*) FROM audit_events WHERE object = ?1 AND action IN ('host.purge', 'host.personal_data.export')",
                [A],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_errors_and_pseudonyms() {
        let dir = setup("usage");
        assert_eq!(run(&dir, &["purge"], None).exit, EXIT_USAGE);
        assert_eq!(run(&dir, &["purge", "not-a-uuid"], None).exit, EXIT_USAGE);
        // 金鑰還不存在
        let o = run(&dir, &["pseudonym", "ann-laptop"], None);
        assert_eq!(o.exit, EXIT_GENERAL);
        let key = crate::privacy::PseudonymKey::load_or_create(&dir.join("data")).unwrap();
        let o = run(&dir, &["pseudonym", "ann-laptop", "db-01"], None);
        assert_eq!(o.exit, 0, "{}", o.stderr);
        assert_eq!(
            o.stdout,
            format!(
                "ann-laptop\t{}\ndb-01\t{}\n",
                key.pseudonym("host", "ann-laptop"),
                key.pseudonym("host", "db-01")
            )
        );
        let o = run(
            &dir,
            &["pseudonym", "ann", "--kind", "user", "--json"],
            None,
        );
        let v: serde_json::Value = serde_json::from_str(o.stdout.trim()).unwrap();
        assert_eq!(
            v["data"]["pseudonyms"][0]["pseudonym"],
            key.pseudonym("user", "ann")
        );
        assert_eq!(
            run(&dir, &["pseudonym", "x", "--kind", "ip"], None).exit,
            EXIT_USAGE
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
