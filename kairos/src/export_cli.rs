//! `kairos reports export`：把 SQLite 中的原始 report 匯出成與 JSONL 檔相同格式的 JSON Lines
//! （`schema` 1／2，見 `kairos/README.md`「資料輸出」），供原本讀取 `reports-*.jsonl` 的工具使用；
//! `kairos db backup`：資料庫的線上備份（`VACUUM INTO`）。
//!
//! 都以唯讀方式開啟資料庫，Kairos 執行中也可以執行。結果寫到 stdout，錯誤寫到 stderr
//! （KRS-E4301「無法產生匯出檔案」）。

use chrono::Utc;
use std::ffi::OsString;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::take_config_arg;
use crate::hosts_cli::{CliError, EXIT_GENERAL, open_db, parse_duration, use_unicode};
use crate::store::sqlite::{backup_to, export_reports};

/// `kairos reports --help`
pub const REPORTS_USAGE: &str = "kairos reports export — 以 JSON Lines 匯出 SQLite 中的原始 report

用法：
  kairos reports export [選項] > reports.jsonl

選項：
  --since <時間>     匯出取樣時間在最近這段時間內的 report，例如 30m、12h、7d（預設 1d）；
                     補送的歷史資料（durable）依 Pulse 的取樣時間 ts，其他依收到的時間
  --agent <agent_id> 只匯出這台主機（未指定時掃描整個 reports 表）
  --config <path>    Kairos 的設定檔（用來找到 storage.data_dir；預設為目前目錄的 kairos.toml）
  -h, --help         顯示本說明

每一行的格式與 reports-YYYY-MM-DD.jsonl 相同（schema 1 為協定 v1 的三欄位，schema 2 為通用指標），
依寫入順序排列；peer 與 hostname 為主機清冊中最後的值（SQLite 不逐筆保存）。
輸出只有監控資料（主機識別、時間與指標），不含憑證、私鑰或設定；檔案權限依目前的 umask。

範例：
  kairos reports export --config /opt/kairos/kairos.toml --since 1d > today.jsonl
  kairos reports export --agent 6f1c0e4a-1b2c-4d3e-9f00-112233445566 --since 30d

結束代碼：0 正常、1 無法讀取資料庫、2 用法錯誤、3 設定錯誤、10 權限不足";

/// `kairos db --help`
pub const DB_USAGE: &str = "kairos db backup — 線上備份 SQLite 資料庫（Kairos 執行中也可以）

用法：
  kairos db backup <備份檔路徑> [--config <path>]

以 SQLite 的 VACUUM INTO 寫出一致的快照（不含空閒頁）。備份檔已存在時不覆寫；Unix 上權限為 0640。
還原：停止 Kairos，把備份檔複製成 <storage.data_dir>/kairos.db（並刪除舊的 kairos.db-wal、kairos.db-shm），
再啟動 Kairos。詳見 STORAGE.md「備份與還原」。

範例：
  sudo -u kairos kairos db backup /var/backups/kairos/kairos-$(date +%F).db --config /opt/kairos/kairos.toml

結束代碼：0 正常、1 備份失敗（例如備份檔已存在、磁碟空間不足）、2 用法錯誤、3 設定錯誤、10 權限不足";

/// `kairos db` 的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbCommand {
    Help,
    Backup {
        config_path: Option<PathBuf>,
        dest: PathBuf,
    },
}

/// 解析 `kairos db` 之後的參數
pub fn parse_db_args(args: impl Iterator<Item = OsString>) -> Result<DbCommand, String> {
    let mut args = args.peekable();
    match args.next() {
        None => return Err("kairos db 需要子命令（目前只有 backup）".to_string()),
        Some(a) if a == "-h" || a == "--help" => return Ok(DbCommand::Help),
        Some(a) if a == "backup" => {}
        Some(a) => {
            return Err(format!(
                "無法辨識的子命令: {}（目前只有 backup）",
                a.to_string_lossy()
            ));
        }
    }
    let mut config_path = None;
    let mut dest = None;
    while let Some(arg) = args.next() {
        if let Some(path) = take_config_arg(&arg, &mut args)? {
            if config_path.replace(path).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        if arg == "-h" || arg == "--help" {
            return Ok(DbCommand::Help);
        }
        if arg.to_string_lossy().starts_with('-') {
            return Err(format!("無法辨識的參數: {}", arg.to_string_lossy()));
        }
        if dest.replace(PathBuf::from(arg)).is_some() {
            return Err("只能指定一個備份檔路徑".to_string());
        }
    }
    let dest = dest.ok_or("kairos db backup 需要備份檔路徑")?;
    Ok(DbCommand::Backup { config_path, dest })
}

/// 執行 `kairos db ...`，回傳結束代碼
pub fn run_db(cmd: DbCommand) -> u8 {
    let (config_path, dest) = match cmd {
        DbCommand::Help => {
            println!("{DB_USAGE}");
            return 0;
        }
        DbCommand::Backup { config_path, dest } => (config_path, dest),
    };
    let code = "KRS-E4301";
    let title = "無法產生備份檔";
    let result = open_db(config_path.as_deref(), code, title).and_then(|conn| {
        backup_to(&conn, &dest).map_err(|e| CliError {
            code,
            title: title.to_string(),
            cause: e.to_string(),
            fix: vec![
                "指定一個不存在的檔案路徑，並確認目的目錄有足夠空間（約為資料庫的使用量）且可寫入。"
                    .to_string(),
            ],
            exit: EXIT_GENERAL,
        })
    });
    match result {
        Ok(()) => {
            let mark = if use_unicode(false) { "✓" } else { "[ OK ]" };
            println!("{mark} 已備份到 {}", dest.display());
            0
        }
        Err(e) => {
            eprint!("{}", e.render_human(use_unicode(false)));
            e.exit
        }
    }
}

/// 預設匯出最近 1 天
const DEFAULT_SINCE: Duration = Duration::from_secs(86_400);

/// `kairos reports` 的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportsCommand {
    Help,
    Export(ExportOptions),
}

/// `kairos reports export` 的選項
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    pub config_path: Option<PathBuf>,
    pub since: Duration,
    pub agent_id: Option<String>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            config_path: None,
            since: DEFAULT_SINCE,
            agent_id: None,
        }
    }
}

/// 解析 `kairos reports` 之後的參數
pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<ReportsCommand, String> {
    let mut args = args.peekable();
    match args.next() {
        None => return Err("kairos reports 需要子命令（目前只有 export）".to_string()),
        Some(a) if a == "-h" || a == "--help" => return Ok(ReportsCommand::Help),
        Some(a) if a == "export" => {}
        Some(a) => {
            return Err(format!(
                "無法辨識的子命令: {}（目前只有 export）",
                a.to_string_lossy()
            ));
        }
    }
    let mut opts = ExportOptions::default();
    while let Some(arg) = args.next() {
        if let Some(path) = take_config_arg(&arg, &mut args)? {
            if opts.config_path.replace(path).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        let Some(a) = arg.to_str().map(str::to_string) else {
            return Err(format!(
                "無法辨識的參數（含有不是有效 Unicode 的字元）: {}",
                arg.to_string_lossy()
            ));
        };
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = || -> Result<String, String> {
            inline
                .clone()
                .or_else(|| args.next().and_then(|v| v.into_string().ok()))
                .ok_or_else(|| format!("{name} 需要指定值"))
        };
        match name.as_str() {
            "-h" | "--help" => return Ok(ReportsCommand::Help),
            "--since" => {
                let v = value()?;
                opts.since = parse_duration(&v)
                    .ok_or_else(|| format!("--since 的值 \"{v}\" 無效，例如 30m、12h、7d"))?;
            }
            "--agent" => {
                let v = value()?;
                if !kairos_proto::is_uuid_text(&v) {
                    return Err(format!("--agent 的值 \"{v}\" 不是 agent_id（UUID）"));
                }
                // 伺服器以小寫保存 agent_id
                opts.agent_id = Some(kairos_proto::normalize_agent_id(&v));
            }
            _ => return Err(format!("無法辨識的參數: {a}")),
        }
    }
    Ok(ReportsCommand::Export(opts))
}

/// 執行 `kairos reports ...`，回傳結束代碼
pub fn run(cmd: ReportsCommand) -> u8 {
    let opts = match cmd {
        ReportsCommand::Help => {
            println!("{REPORTS_USAGE}");
            return 0;
        }
        ReportsCommand::Export(opts) => opts,
    };
    let title = "無法產生匯出檔案";
    let code = "KRS-E4301";
    let result = open_db(opts.config_path.as_deref(), code, title).and_then(|conn| {
        let since = Utc::now() - chrono::Duration::from_std(opts.since).unwrap_or_default();
        let stdout = std::io::stdout().lock();
        let mut out = BufWriter::new(stdout);
        let mut line = Vec::with_capacity(1024);
        let exported = export_reports(
            &conn,
            since.timestamp_millis(),
            opts.agent_id.as_deref(),
            |record| {
                line.clear();
                serde_json::to_writer(&mut line, record).map_err(std::io::Error::other)?;
                line.push(b'\n');
                out.write_all(&line)
            },
        )
        .and_then(|n| out.flush().map(|_| n));
        exported.map_err(|e| CliError {
            code,
            title: title.to_string(),
            cause: e.to_string(),
            fix: vec!["確認 storage.data_dir 指向 Kairos 的資料目錄；資料庫損毀時請從備份還原（kairos/STORAGE.md「備份與還原」）。".to_string()],
            exit: if e.kind() == ErrorKind::BrokenPipe { 0 } else { EXIT_GENERAL },
        })
    });
    match result {
        Ok(_) => 0,
        // 輸出被關閉（例如 | head）不視為錯誤
        Err(e) if e.exit == 0 => 0,
        Err(e) => {
            eprint!("{}", e.render_human(use_unicode(false)));
            e.exit
        }
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
        assert_eq!(parse_args(os(&["--help"])), Ok(ReportsCommand::Help));
        assert_eq!(
            parse_args(os(&["export"])),
            Ok(ReportsCommand::Export(ExportOptions::default()))
        );
        assert_eq!(
            parse_args(os(&[
                "export",
                "--since=12h",
                "--agent",
                "6F1C0E4A-1B2C-4D3E-9F00-112233445566",
                "--config",
                "k.toml"
            ])),
            Ok(ReportsCommand::Export(ExportOptions {
                config_path: Some(PathBuf::from("k.toml")),
                since: Duration::from_secs(12 * 3600),
                agent_id: Some("6f1c0e4a-1b2c-4d3e-9f00-112233445566".into()),
            }))
        );
        for bad in [
            &[][..],
            &["import"],
            &["export", "--since"],
            &["export", "--since", "yesterday"],
            &["export", "--agent", "web-01"],
            &["export", "--json"],
        ] {
            assert!(parse_args(os(bad)).is_err(), "{bad:?}");
        }
        assert!(matches!(
            crate::config::parse_args(os(&["reports", "export"])),
            Ok(crate::config::CliAction::Reports(ReportsCommand::Export(_)))
        ));
    }

    #[test]
    fn db_argument_parsing() {
        assert_eq!(parse_db_args(os(&["--help"])), Ok(DbCommand::Help));
        assert_eq!(
            parse_db_args(os(&["backup", "b.db", "--config", "k.toml"])),
            Ok(DbCommand::Backup {
                config_path: Some(PathBuf::from("k.toml")),
                dest: PathBuf::from("b.db"),
            })
        );
        for bad in [
            &[][..],
            &["restore"],
            &["backup"],
            &["backup", "a.db", "b.db"],
            &["backup", "--force", "a.db"],
        ] {
            assert!(parse_db_args(os(bad)).is_err(), "{bad:?}");
        }
        assert!(matches!(
            crate::config::parse_args(os(&["db", "backup", "x.db"])),
            Ok(crate::config::CliAction::Db(DbCommand::Backup { .. }))
        ));
    }
}
