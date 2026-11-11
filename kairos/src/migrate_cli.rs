//! `kairos db migrate`：SQLite 與 PostgreSQL 互搬的命令列（`kairos/HA.md` 第 5.6 節；引擎在
//! [`crate::store::migrate`]）。
//!
//! 來源是設定中目前的後端（`storage.backend`），目的由 `--to` 指定：
//!
//! - `--to postgres`：設定的 `storage.backend` 必須是 `sqlite`，PostgreSQL 的連線參數取自
//!   `[storage.postgres]`（搬移完成後再把 `storage.backend` 改成 `"postgres"`）；
//! - `--to sqlite:<檔案>`：設定的 `storage.backend` 必須是 `postgres`。

use chrono::Utc;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{AppConfig, StorageBackend};
use crate::hosts_cli::{CliError, EXIT_GENERAL, parse_duration, use_unicode};
use crate::store::migrate::{self, MigrateError, Options, Summary, TableResult};
use crate::store::postgres::pool::{self, PgPool};
use crate::store::sqlite::SqliteOptions;

/// `--json` 輸出的 schema
pub const JSON_SCHEMA: &str = "kairos.db.migrate/v1";

const EXIT_USAGE_HINT: &str = "kairos db migrate --help";
const EXIT_CONFIG: u8 = 3;

pub const MIGRATE_USAGE: &str = "kairos db migrate — SQLite 與 PostgreSQL 互搬（停止 Kairos 後執行）

用法：
  kairos db migrate --to postgres [--reports-since <期間>] [--dry-run] [--restart] [--json] [--config <path>]
  kairos db migrate --to sqlite:<檔案> [--reports-since <期間>] [--dry-run] [--restart] [--json] [--config <path>]

來源是設定中目前的後端（storage.backend），目的由 --to 指定：
  --to postgres         SQLite（<storage.data_dir>/kairos.db）→ PostgreSQL（[storage.postgres] 的資料庫與 schema）。
                        設定的 storage.backend 仍是 \"sqlite\"；搬移成功後再改成 \"postgres\" 並啟動 Kairos。
  --to sqlite:<檔案>    PostgreSQL → 新的 SQLite 檔（回到單機，或匯出成可攜的快照）。檔案不可已存在。
目的端必須是空的（不做合併）。資料表依相依順序複製並保留所有主鍵值；完成後一定會驗證：
每張表的列數、reports 的 id 範圍與抽樣 1000 列的內容、稽核鏈（與來源的驗證結果相同）。
驗證不一致時目的端不可使用（PostgreSQL 不寫入 schema 版本、SQLite 留在 <檔案>.partial）。

  --reports-since <期間>  只複製最近這段時間的原始 report 與行程快照（例 2d、12h）；彙總、清冊、告警、稽核等
                          一律全部複製。範圍外的原始資料留在來源，可另以 kairos reports export 取出
  --dry-run               只檢查並列出每張表的列數，不寫入
  --restart               目的端有上次未完成的搬移時先清空再執行
  --json                  以 JSON 輸出結果（schema kairos.db.migrate/v1）

範例：
  sudo systemctl stop kairos
  sudo -u kairos kairos db migrate --to postgres --config /opt/kairos/kairos.toml
  sudo -u kairos kairos db migrate --to sqlite:/var/backups/kairos/export.db --config /opt/kairos/kairos.toml

結束代碼：0 正常、1 失敗（KRS-E4304 資料庫正在使用中；KRS-E4404 搬移失敗或驗證不一致；KRS-E4401 無法連線）、
2 用法錯誤、3 設定錯誤";

/// 目的端
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Postgres,
    Sqlite(PathBuf),
}

/// `kairos db migrate` 的參數
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateArgs {
    pub config_path: Option<PathBuf>,
    pub to: Target,
    pub reports_since: Option<Duration>,
    pub dry_run: bool,
    pub restart: bool,
    pub json: bool,
}

/// 解析 `kairos db migrate` 之後的參數；`None` 表示要求說明
pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Option<MigrateArgs>, String> {
    let mut args = args.peekable();
    let mut config_path = None;
    let mut to = None;
    let mut reports_since = None;
    let (mut dry_run, mut restart, mut json) = (false, false, false);
    let parse_to = |text: &str| -> Result<Target, String> {
        if text == "postgres" {
            Ok(Target::Postgres)
        } else if let Some(path) = text.strip_prefix("sqlite:").filter(|p| !p.is_empty()) {
            Ok(Target::Sqlite(PathBuf::from(path)))
        } else {
            Err(format!(
                "--to 的值「{text}」不合法：請用 postgres 或 sqlite:<檔案>"
            ))
        }
    };
    let parse_since = |text: &str| -> Result<Duration, String> {
        parse_duration(text)
            .filter(|d| !d.is_zero())
            .ok_or_else(|| format!("--reports-since 的值「{text}」不合法（例 2d、12h、90m）"))
    };
    while let Some(arg) = args.next() {
        if let Some(p) = crate::config::take_config_arg(&arg, &mut args)? {
            if config_path.replace(p).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        let text = arg.to_string_lossy().into_owned();
        let mut value_of = |name: &str| -> Result<String, String> {
            match args.next() {
                Some(v) if !v.is_empty() => Ok(v.to_string_lossy().into_owned()),
                _ => Err(format!("{name} 需要一個值")),
            }
        };
        match text.as_str() {
            "-h" | "--help" => return Ok(None),
            "--dry-run" => dry_run = true,
            "--restart" => restart = true,
            "--json" => json = true,
            "--to" => to = Some(parse_to(&value_of("--to")?)?),
            "--reports-since" => reports_since = Some(parse_since(&value_of("--reports-since")?)?),
            _ => {
                if let Some(v) = text.strip_prefix("--to=") {
                    to = Some(parse_to(v)?);
                } else if let Some(v) = text.strip_prefix("--reports-since=") {
                    reports_since = Some(parse_since(v)?);
                } else {
                    return Err(format!("無法辨識的參數: {text}"));
                }
            }
        }
    }
    let to = to.ok_or("kairos db migrate 需要 --to postgres 或 --to sqlite:<檔案>")?;
    Ok(Some(MigrateArgs {
        config_path,
        to,
        reports_since,
        dry_run,
        restart,
        json,
    }))
}

fn error_of(e: MigrateError, title: &str) -> CliError {
    let fix = match e.code {
        migrate::CODE_IN_USE => vec![
            "停止 Kairos（叢集時停止所有節點）後再執行：sudo systemctl stop kairos".to_string(),
        ],
        pool::CODE_PG_UNREACHABLE => vec![
            "檢查網路、[storage.postgres] 的 host／port／dbname／user、password_file 與 TLS；可先執行 kairos --check-config。".to_string(),
        ],
        _ if e.config => vec![format!("修正後重新執行；用法見 {EXIT_USAGE_HINT}。")],
        _ => vec![
            "依訊息修正後以 --restart 重新執行（目的端會先清空）。".to_string(),
            "來源的資料沒有被修改，可以照常以原本的後端啟動 Kairos。".to_string(),
        ],
    };
    CliError {
        code: e.code,
        title: title.to_string(),
        cause: e.message,
        fix,
        exit: if e.config { EXIT_CONFIG } else { EXIT_GENERAL },
    }
}

fn render_error_json(e: &CliError) -> String {
    serde_json::json!({
        "schema": JSON_SCHEMA,
        "ok": false,
        "generated_at": crate::storage::fmt_time(Utc::now()),
        "data": null,
        "error": { "code": e.code, "title": e.title, "cause": e.cause, "fix": e.fix },
    })
    .to_string()
        + "\n"
}

fn render_json(args: &MigrateArgs, from: &str, to: &str, s: &Summary) -> String {
    serde_json::json!({
        "schema": JSON_SCHEMA,
        "ok": true,
        "generated_at": crate::storage::fmt_time(Utc::now()),
        "data": {
            "from": from,
            "to": to,
            "dry_run": s.dry_run,
            "reports_since_secs": args.reports_since.map(|d| d.as_secs()),
            "tables": s.tables.iter().map(|t| serde_json::json!({
                "name": t.name, "source_rows": t.source_rows, "copied_rows": t.copied_rows,
            })).collect::<Vec<_>>(),
            "total_rows": s.total_rows,
            "verification": {
                "row_counts": !s.dry_run,
                "reports_sampled": s.sampled,
                "audit_checked": s.audit_checked,
                "audit_intact": s.audit_intact,
            },
            "elapsed_ms": u64::try_from(s.elapsed.as_millis()).unwrap_or(u64::MAX),
        },
        "error": null,
    })
    .to_string()
        + "\n"
}

/// 執行；回傳結束代碼
pub fn run(args: MigrateArgs) -> u8 {
    let unicode = use_unicode(false);
    let title = "資料搬移失敗";
    let fail = |e: CliError| -> u8 {
        if args.json {
            print!("{}", render_error_json(&e));
        } else {
            eprint!("{}", e.render_human(unicode));
        }
        e.exit
    };
    let config_error = |cause: String, fix: &str| CliError {
        code: migrate::CODE_FAILED,
        title: title.to_string(),
        cause,
        fix: vec![fix.to_string()],
        exit: EXIT_CONFIG,
    };
    let config = match AppConfig::load(args.config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            return fail(CliError {
                code: crate::diag::config_load_code(&e, args.config_path.as_deref()),
                title: "設定檔無法載入".to_string(),
                cause: e.to_string(),
                fix: vec![
                    "修正設定檔後再執行，或以 --config 指定 Kairos 使用的設定檔。".to_string(),
                ],
                exit: EXIT_CONFIG,
            });
        }
    };
    let data_dir = PathBuf::from(&config.storage.data_dir);
    let sqlite_opts = SqliteOptions::from_config(&config, &data_dir);
    let key = crate::audit::key_for_db_path(Some(&sqlite_opts.path.display().to_string()));
    let options = Options {
        reports_since_ms: args.reports_since.map(|d| {
            Utc::now().timestamp_millis() - i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
        }),
        dry_run: args.dry_run,
        restart: args.restart,
    };
    let mark = if unicode { "✓" } else { "[ OK ]" };
    let json = args.json;
    let dry = args.dry_run;
    let mut progress = |t: &TableResult| {
        if !json {
            if dry {
                println!("  {:<26} {:>12} 列", t.name, t.source_rows);
            } else {
                println!("{mark} {:<26} {:>12} 列", t.name, t.copied_rows);
            }
        }
    };
    let (from, to, result) = match (&args.to, config.storage.backend) {
        (Target::Postgres, StorageBackend::Sqlite) => {
            if let Err(e) = config.storage.postgres.validate() {
                return fail(config_error(
                    e.to_string(),
                    "在設定檔加入 [storage.postgres]（host、dbname、user、password_file…）；storage.backend 維持 \"sqlite\"，搬移成功後再改。",
                ));
            }
            let pg = PgPool::with_data_dir(config.storage.postgres.clone(), Some(data_dir.clone()));
            if let Err(e) = crate::store::postgres::check_server(&pg) {
                let text = e.to_string();
                let code = if text.contains(pool::CODE_PG_REQUIREMENTS) {
                    pool::CODE_PG_REQUIREMENTS
                } else {
                    pool::CODE_PG_UNREACHABLE
                };
                return fail(error_of(
                    MigrateError {
                        code,
                        message: text,
                        config: false,
                    },
                    title,
                ));
            }
            let from = sqlite_opts.path.display().to_string();
            let to = pg.settings().display_target();
            if !json {
                println!("搬移 {from} → {to}{}", if dry { "（試執行）" } else { "" });
            }
            let r = migrate::sqlite_to_postgres(
                &sqlite_opts,
                &pg,
                &options,
                key.as_deref(),
                &mut progress,
            );
            (from, to, r)
        }
        (Target::Sqlite(dest), StorageBackend::Postgres) => {
            let Some(pg) = pool::active() else {
                return fail(config_error(
                    "PostgreSQL 的連線設定無法使用".to_string(),
                    "檢查 [storage.postgres]。",
                ));
            };
            let pg: Arc<PgPool> = pg;
            let from = pg.settings().display_target();
            let to = dest.display().to_string();
            if !json {
                println!("搬移 {from} → {to}{}", if dry { "（試執行）" } else { "" });
            }
            let r = migrate::postgres_to_sqlite(
                &pg,
                dest,
                &sqlite_opts,
                &options,
                key.as_deref(),
                &mut progress,
            );
            (from, to, r)
        }
        (Target::Postgres, other) => {
            return fail(config_error(
                format!(
                    "--to postgres 的來源必須是 SQLite，但設定的 storage.backend 是 \"{}\"。",
                    other.as_str()
                ),
                "來源是設定中目前的後端：搬到 PostgreSQL 時 storage.backend 要維持 \"sqlite\"，[storage.postgres] 填目的資料庫；搬移成功後再改成 \"postgres\"。",
            ));
        }
        (Target::Sqlite(_), other) => {
            return fail(config_error(
                format!(
                    "--to sqlite:<檔案> 的來源必須是 PostgreSQL，但設定的 storage.backend 是 \"{}\"。",
                    other.as_str()
                ),
                "來源是設定中目前的後端：從 PostgreSQL 搬回 SQLite 時 storage.backend 要是 \"postgres\"。",
            ));
        }
    };
    match result {
        Ok(summary) => {
            if json {
                print!("{}", render_json(&args, &from, &to, &summary));
            } else if summary.dry_run {
                println!(
                    "試執行：共 {} 列會被複製；稽核鏈 {} 筆{}。沒有寫入任何東西。",
                    summary.total_rows,
                    summary.audit_checked,
                    if summary.audit_intact {
                        "，完整"
                    } else {
                        "，來源的鏈已有斷點（搬移後仍會如此）"
                    }
                );
            } else {
                println!(
                    "{mark} 驗證通過：{} 張表的列數一致、reports 抽樣比對 {} 列、稽核鏈 {} 筆與來源相同{}",
                    summary.tables.len(),
                    summary.sampled,
                    summary.audit_checked,
                    if summary.audit_intact {
                        ""
                    } else {
                        "（來源的鏈原本就有斷點）"
                    }
                );
                println!(
                    "{mark} 已搬移 {} 列，耗時 {:.1} 秒",
                    summary.total_rows,
                    summary.elapsed.as_secs_f64()
                );
                match args.to {
                    Target::Postgres => println!(
                        "  下一步：把設定檔的 storage.backend 改成 \"postgres\"，執行 kairos --check-config 後啟動 Kairos。原本的 SQLite 資料庫沒有被修改，確認無誤後再自行封存。"
                    ),
                    Target::Sqlite(ref p) => println!(
                        "  下一步：要回到單機時，把 {} 放到 <storage.data_dir>/kairos.db，設定檔改回 storage.backend = \"sqlite\" 後啟動 Kairos。",
                        p.display()
                    ),
                }
            }
            0
        }
        Err(e) => fail(error_of(e, title)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> impl Iterator<Item = OsString> {
        args.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn arguments_are_parsed() {
        assert_eq!(parse_args(os(&["--help"])), Ok(None));
        assert_eq!(
            parse_args(os(&["--to", "postgres"])),
            Ok(Some(MigrateArgs {
                config_path: None,
                to: Target::Postgres,
                reports_since: None,
                dry_run: false,
                restart: false,
                json: false,
            }))
        );
        assert_eq!(
            parse_args(os(&[
                "--to=sqlite:/tmp/x.db",
                "--reports-since",
                "2d",
                "--dry-run",
                "--restart",
                "--json",
                "--config",
                "k.toml"
            ])),
            Ok(Some(MigrateArgs {
                config_path: Some(PathBuf::from("k.toml")),
                to: Target::Sqlite(PathBuf::from("/tmp/x.db")),
                reports_since: Some(Duration::from_secs(2 * 86_400)),
                dry_run: true,
                restart: true,
                json: true,
            }))
        );
    }

    #[test]
    fn invalid_arguments_are_rejected() {
        for (args, needle) in [
            (vec![], "--to"),
            (vec!["--to", "mysql"], "不合法"),
            (vec!["--to", "sqlite:"], "不合法"),
            (vec!["--to"], "需要一個值"),
            (
                vec!["--to", "postgres", "--reports-since", "abc"],
                "--reports-since",
            ),
            (
                vec!["--to", "postgres", "--reports-since", "0d"],
                "--reports-since",
            ),
            (vec!["--to", "postgres", "--force"], "無法辨識"),
        ] {
            let e = parse_args(os(&args)).unwrap_err();
            assert!(e.contains(needle), "{args:?}: {e}");
        }
    }
}
