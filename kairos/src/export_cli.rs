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
use crate::store::db::StoreError;

/// `kairos reports --help`
pub const REPORTS_USAGE: &str = "kairos reports export — 以 JSON Lines 匯出 SQLite 中的原始 report

用法：
  kairos reports export [選項] > reports.jsonl
  kairos reports import [選項] <檔案或目錄>...（匯入 0.2.x 的 JSONL；kairos reports import --help 顯示說明）

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
pub const DB_USAGE: &str = "kairos db backup|restore-keys — 線上備份 SQLite 資料庫；金鑰檔的加密備份與還原

用法：
  kairos db backup <備份檔路徑> [--include-keys (--password-file <檔案> | --password-stdin)] [--config <path>]
  kairos db restore-keys <金鑰備份檔> (--password-file <檔案> | --password-stdin) [--overwrite] [--config <path>]

backup：以 SQLite 的 VACUUM INTO 寫出一致的快照（不含空閒頁）。備份檔已存在時不覆寫；Unix 上權限為 0640。
  資料庫備份不含資料目錄的金鑰檔（alerts-secrets.key、console.key、audit-hmac.key）。
  --include-keys      另把存在的金鑰檔以備份密碼加密（Argon2id＋AES-256-GCM），寫成 <備份檔路徑>.keys（0600）
  --password-file     從檔案讀取備份密碼（第一行；至少 12 個字元）
  --password-stdin    從 stdin 讀取備份密碼（不接受命令列上的密碼）
restore-keys：解開金鑰備份並寫回 <storage.data_dir>（0600；以 root 執行時擁有者改為資料目錄的擁有者）。
  請在 Kairos 停止時執行。資料目錄已有內容不同的金鑰檔時拒絕；--overwrite 時舊檔改名為 <名稱>.replaced-<時間> 保留。
還原資料庫：停止 Kairos，把備份檔複製成 <storage.data_dir>/kairos.db（並刪除舊的 kairos.db-wal、kairos.db-shm），
需要時執行 restore-keys，再啟動 Kairos。詳見 STORAGE.md「備份與還原」。

範例：
  sudo -u kairos kairos db backup /var/backups/kairos/kairos-$(date +%F).db --config /opt/kairos/kairos.toml
  sudo -u kairos kairos db backup /var/backups/kairos/kairos-$(date +%F).db --include-keys --password-file /root/kairos-backup.pass --config /opt/kairos/kairos.toml
  sudo -u kairos kairos db restore-keys /var/backups/kairos/kairos-2026-10-15.db.keys --password-stdin --config /opt/kairos/kairos.toml

結束代碼：0 正常、1 失敗（例如備份檔已存在、磁碟空間不足、密碼錯誤或金鑰備份被修改 KRS-E4307、
金鑰檔已存在且不同）、2 用法錯誤、3 設定錯誤、10 權限不足";

/// 備份密碼的來源（不接受命令列上的密碼）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordSource {
    File(PathBuf),
    Stdin,
}

/// `kairos db` 的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbCommand {
    Help,
    Backup {
        config_path: Option<PathBuf>,
        dest: PathBuf,
        /// `--include-keys`：金鑰檔的加密備份與密碼來源
        keys: Option<PasswordSource>,
    },
    RestoreKeys {
        config_path: Option<PathBuf>,
        source: PathBuf,
        password: PasswordSource,
        overwrite: bool,
    },
}

/// 解析 `kairos db` 之後的參數
pub fn parse_db_args(args: impl Iterator<Item = OsString>) -> Result<DbCommand, String> {
    let mut args = args.peekable();
    let restore = match args.next() {
        None => return Err("kairos db 需要子命令（backup、restore-keys）".to_string()),
        Some(a) if a == "-h" || a == "--help" => return Ok(DbCommand::Help),
        Some(a) if a == "backup" => false,
        Some(a) if a == "restore-keys" => true,
        Some(a) => {
            return Err(format!(
                "無法辨識的子命令: {}（backup、restore-keys）",
                a.to_string_lossy()
            ));
        }
    };
    let mut config_path = None;
    let mut path = None;
    let mut include_keys = false;
    let mut overwrite = false;
    let mut password: Option<PasswordSource> = None;
    let mut set_password = |p: PasswordSource| -> Result<(), String> {
        if password.replace(p).is_some() {
            return Err("--password-file、--password-stdin 只能擇一指定一次".to_string());
        }
        Ok(())
    };
    while let Some(arg) = args.next() {
        if let Some(p) = take_config_arg(&arg, &mut args)? {
            if config_path.replace(p).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        if arg == "-h" || arg == "--help" {
            return Ok(DbCommand::Help);
        }
        let text = arg.to_string_lossy();
        if text == "--include-keys" && !restore {
            include_keys = true;
        } else if text == "--overwrite" && restore {
            overwrite = true;
        } else if text == "--password-stdin" {
            set_password(PasswordSource::Stdin)?;
        } else if text == "--password-file" {
            match args.next() {
                Some(p) if !p.is_empty() => set_password(PasswordSource::File(PathBuf::from(p)))?,
                _ => return Err("--password-file 需要指定檔案路徑".to_string()),
            }
        } else if let Some(p) = text.strip_prefix("--password-file=") {
            if p.is_empty() {
                return Err("--password-file 需要指定檔案路徑".to_string());
            }
            set_password(PasswordSource::File(PathBuf::from(p)))?;
        } else if text == "--password" || text.starts_with("--password=") {
            return Err(
                "不接受命令列上的備份密碼（會留在 shell 歷史與行程清單）；請改用 --password-file <檔案> 或 --password-stdin"
                    .to_string(),
            );
        } else if text.starts_with('-') {
            return Err(format!("無法辨識的參數: {text}"));
        } else if path.replace(PathBuf::from(arg)).is_some() {
            return Err(if restore {
                "只能指定一個金鑰備份檔".to_string()
            } else {
                "只能指定一個備份檔路徑".to_string()
            });
        }
    }
    if restore {
        let source = path.ok_or("kairos db restore-keys 需要金鑰備份檔（<備份檔>.keys）")?;
        let password = password
            .ok_or("kairos db restore-keys 需要 --password-file <檔案> 或 --password-stdin")?;
        return Ok(DbCommand::RestoreKeys {
            config_path,
            source,
            password,
            overwrite,
        });
    }
    let dest = path.ok_or("kairos db backup 需要備份檔路徑")?;
    let keys = match (include_keys, password) {
        (true, Some(p)) => Some(p),
        (true, None) => {
            return Err(
                "--include-keys 需要 --password-file <檔案> 或 --password-stdin（金鑰檔一律以備份密碼加密）"
                    .to_string(),
            );
        }
        (false, Some(_)) => {
            return Err("--password-file、--password-stdin 只在 --include-keys 時使用".to_string());
        }
        (false, None) => None,
    };
    Ok(DbCommand::Backup {
        config_path,
        dest,
        keys,
    })
}

fn read_password(src: &PasswordSource) -> Result<crate::keys_backup::Password, String> {
    match src {
        PasswordSource::File(p) => crate::keys_backup::Password::from_file(p),
        PasswordSource::Stdin => crate::keys_backup::Password::from_reader(std::io::stdin().lock()),
    }
}

fn keys_error(title: &str, cause: impl Into<String>, fix: Vec<String>) -> CliError {
    CliError {
        code: crate::keys_backup::CODE_KEYS_BACKUP,
        title: title.to_string(),
        cause: cause.into(),
        fix,
        exit: EXIT_GENERAL,
    }
}

/// 資料目錄（設定檔的 storage.data_dir）
fn data_dir_of(config_path: Option<&std::path::Path>) -> Result<PathBuf, CliError> {
    let config = crate::config::AppConfig::load(config_path).map_err(|e| CliError {
        code: crate::diag::config_load_code(&e, config_path),
        title: "設定檔無法載入".to_string(),
        cause: e.to_string(),
        fix: vec!["修正設定檔後再執行，或以 --config 指定 Kairos 使用的設定檔。".to_string()],
        exit: 3,
    })?;
    Ok(PathBuf::from(config.storage.data_dir))
}

/// `kairos db backup --include-keys`：先讀密碼與金鑰並寫出 `.keys`，再備份資料庫（失敗時刪除 `.keys`）
fn backup_keys(
    config_path: Option<&std::path::Path>,
    dest: &std::path::Path,
    src: &PasswordSource,
) -> Result<(PathBuf, Vec<String>, Vec<&'static str>), CliError> {
    use crate::keys_backup as kb;
    let title = "無法產生金鑰備份";
    let password = read_password(src).map_err(|e| {
        keys_error(
            title,
            e,
            vec![format!(
                "備份密碼至少 {} 個字元，以 --password-file 或 --password-stdin 提供；請與備份分開保管。",
                kb::MIN_PASSWORD_CHARS
            )],
        )
    })?;
    let data_dir = data_dir_of(config_path)?;
    let (files, missing) = kb::read_keys(&data_dir).map_err(|e| {
        keys_error(
            title,
            e.to_string(),
            vec!["以服務帳號執行（sudo -u kairos），並確認 storage.data_dir 指向 Kairos 的資料目錄。".to_string()],
        )
    })?;
    let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
    let text = kb::seal(&files, &password, kb::DEFAULT_KDF)
        .map_err(|e| keys_error(title, e, Vec::new()))?;
    let out = kb::keys_path(dest);
    kb::write_new_private(&out, text.as_bytes()).map_err(|e| {
        keys_error(
            title,
            e.to_string(),
            vec!["指定一個不存在的備份檔路徑（金鑰備份寫在 <備份檔路徑>.keys）。".to_string()],
        )
    })?;
    Ok((out, names, missing))
}

/// `kairos db restore-keys`
fn restore_keys(
    config_path: Option<&std::path::Path>,
    source: &std::path::Path,
    src: &PasswordSource,
    overwrite: bool,
) -> Result<Vec<(String, crate::keys_backup::Restored)>, CliError> {
    use crate::keys_backup as kb;
    let title = "無法還原金鑰檔";
    let text = std::fs::read_to_string(source).map_err(|e| {
        keys_error(
            title,
            format!("無法讀取 {}：{e}", source.display()),
            Vec::new(),
        )
    })?;
    let password = read_password(src).map_err(|e| keys_error(title, e, Vec::new()))?;
    let files = kb::open(&text, &password).map_err(|e| {
        keys_error(
            title,
            e,
            vec![
                "確認使用的是建立這份備份時的備份密碼（密碼錯誤與檔案被修改無法區分）。".to_string(),
                "確認檔案是 kairos db backup --include-keys 產生的 <備份檔>.keys，且沒有被修改或截斷。".to_string(),
            ],
        )
    })?;
    let data_dir = data_dir_of(config_path)?;
    kb::restore(&data_dir, &files, overwrite).map_err(|e| {
        keys_error(
            title,
            e.to_string(),
            if e.kind() == ErrorKind::AlreadyExists {
                vec![
                    "確認資料庫是從同一份備份還原後，再加 --overwrite（舊的金鑰檔會改名保留）。"
                        .to_string(),
                ]
            } else {
                vec![
                    "以服務帳號（sudo -u kairos）或 root 執行，並確認 storage.data_dir 可寫入。"
                        .to_string(),
                ]
            },
        )
    })
}

/// 執行 `kairos db ...`，回傳結束代碼
pub fn run_db(cmd: DbCommand) -> u8 {
    let unicode = use_unicode(false);
    let mark = if unicode { "✓" } else { "[ OK ]" };
    let (config_path, dest, keys) = match cmd {
        DbCommand::Help => {
            println!("{DB_USAGE}");
            return 0;
        }
        DbCommand::Backup {
            config_path,
            dest,
            keys,
        } => (config_path, dest, keys),
        DbCommand::RestoreKeys {
            config_path,
            source,
            password,
            overwrite,
        } => {
            return match restore_keys(config_path.as_deref(), &source, &password, overwrite) {
                Ok(done) => {
                    for (name, r) in &done {
                        match r {
                            crate::keys_backup::Restored::Written => {
                                println!("{mark} 已還原 {name}（0600）")
                            }
                            crate::keys_backup::Restored::Unchanged => {
                                println!("{mark} {name} 與備份相同，未變更")
                            }
                            crate::keys_backup::Restored::Replaced(kept) => println!(
                                "{mark} 已還原 {name}（0600）；原本的檔案改名為 {}",
                                kept.display()
                            ),
                        }
                    }
                    println!(
                        "  下一步：啟動 Kairos，並執行 kairos audit verify 確認稽核鏈與 HMAC。"
                    );
                    0
                }
                Err(e) => {
                    eprint!("{}", e.render_human(unicode));
                    e.exit
                }
            };
        }
    };
    let code = "KRS-E4301";
    let title = "無法產生備份檔";
    // 金鑰備份先做（密碼不符合要求、金鑰檔無法讀取時不留下資料庫備份）
    let keys_done = match &keys {
        Some(src) => match backup_keys(config_path.as_deref(), &dest, src) {
            Ok(done) => Some(done),
            Err(e) => {
                eprint!("{}", e.render_human(unicode));
                return e.exit;
            }
        },
        None => None,
    };
    let result = open_db(config_path.as_deref(), code, title).and_then(|conn| {
        conn.backup_to(&dest).map_err(|e| CliError {
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
            println!("{mark} 已備份到 {}", dest.display());
            match keys_done {
                Some((out, names, missing)) => {
                    println!(
                        "{mark} 金鑰檔已加密備份到 {}（{}；0600）",
                        out.display(),
                        if names.is_empty() {
                            "資料目錄沒有金鑰檔".to_string()
                        } else {
                            names.join("、")
                        }
                    );
                    if !missing.is_empty() {
                        println!("  不存在而未包含：{}", missing.join("、"));
                    }
                    println!("  備份密碼遺失時無法解開；請把密碼與備份分開保管。");
                }
                None => println!(
                    "  不含資料目錄的金鑰檔（alerts-secrets.key、console.key、audit-hmac.key）；需要時加 --include-keys。"
                ),
            }
            0
        }
        Err(e) => {
            if let Some((out, ..)) = &keys_done {
                let _ = std::fs::remove_file(out);
            }
            eprint!("{}", e.render_human(unicode));
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
    /// `kairos reports import --help`
    ImportHelp,
    /// `kairos reports import`（見 [`crate::import_cli`]）
    Import(crate::import_cli::ImportOptions),
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
        None => return Err("kairos reports 需要子命令（export 或 import）".to_string()),
        Some(a) if a == "-h" || a == "--help" => return Ok(ReportsCommand::Help),
        Some(a) if a == "export" => {}
        Some(a) if a == "import" => {
            return Ok(crate::import_cli::parse_args(&mut args)?
                .map_or(ReportsCommand::ImportHelp, ReportsCommand::Import));
        }
        Some(a) => {
            return Err(format!(
                "無法辨識的子命令: {}（export 或 import）",
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
        ReportsCommand::ImportHelp => {
            println!("{}", crate::import_cli::IMPORT_USAGE);
            return 0;
        }
        ReportsCommand::Import(opts) => return crate::import_cli::run(opts),
        ReportsCommand::Export(opts) => opts,
    };
    let title = "無法產生匯出檔案";
    let code = "KRS-E4301";
    let result = open_db(opts.config_path.as_deref(), code, title).and_then(|conn| {
        let since = Utc::now() - chrono::Duration::from_std(opts.since).unwrap_or_default();
        let stdout = std::io::stdout().lock();
        let mut out = BufWriter::new(stdout);
        let mut line = Vec::with_capacity(1024);
        let exported = conn
            .export_reports(
                since.timestamp_millis(),
                opts.agent_id.as_deref(),
                &mut |record| {
                    line.clear();
                    serde_json::to_writer(&mut line, record).map_err(std::io::Error::other)?;
                    line.push(b'\n');
                    out.write_all(&line)
                },
            )
            .and_then(|n| out.flush().map(|_| n).map_err(StoreError::from));
        exported.map_err(|e| CliError {
            code,
            title: title.to_string(),
            cause: e.to_string(),
            fix: vec!["確認 storage.data_dir 指向 Kairos 的資料目錄；資料庫損毀時請從備份還原（kairos/STORAGE.md「備份與還原」）。".to_string()],
            exit: if e.io_kind() == Some(ErrorKind::BrokenPipe) { 0 } else { EXIT_GENERAL },
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
            &["delete"],
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
                keys: None,
            })
        );
        assert_eq!(
            parse_db_args(os(&[
                "backup",
                "b.db",
                "--include-keys",
                "--password-file",
                "p.txt"
            ])),
            Ok(DbCommand::Backup {
                config_path: None,
                dest: PathBuf::from("b.db"),
                keys: Some(PasswordSource::File(PathBuf::from("p.txt"))),
            })
        );
        assert_eq!(
            parse_db_args(os(&[
                "restore-keys",
                "b.db.keys",
                "--password-stdin",
                "--overwrite"
            ])),
            Ok(DbCommand::RestoreKeys {
                config_path: None,
                source: PathBuf::from("b.db.keys"),
                password: PasswordSource::Stdin,
                overwrite: true,
            })
        );
        // 命令列上的密碼一律拒絕，並說明替代方式
        let e = parse_db_args(os(&[
            "backup",
            "b.db",
            "--include-keys",
            "--password=hunter2hunter2",
        ]))
        .unwrap_err();
        assert!(e.contains("--password-file"), "{e}");
        for bad in [
            &[][..],
            &["restore"],
            &["backup"],
            &["backup", "a.db", "b.db"],
            &["backup", "--force", "a.db"],
            // --include-keys 需要密碼來源；密碼來源只在 --include-keys 時使用
            &["backup", "a.db", "--include-keys"],
            &["backup", "a.db", "--password-stdin"],
            &[
                "backup",
                "a.db",
                "--include-keys",
                "--password-stdin",
                "--password-file",
                "p",
            ],
            &["backup", "a.db", "--overwrite"],
            &["restore-keys", "a.keys"],
            &["restore-keys", "--password-stdin"],
            &[
                "restore-keys",
                "a.keys",
                "--include-keys",
                "--password-stdin",
            ],
        ] {
            assert!(parse_db_args(os(bad)).is_err(), "{bad:?}");
        }
        assert!(matches!(
            crate::config::parse_args(os(&["db", "backup", "x.db"])),
            Ok(crate::config::CliAction::Db(DbCommand::Backup { .. }))
        ));
    }

    /// `kairos db backup --include-keys` → 刪除金鑰檔 → `kairos db restore-keys`：內容相同、0600；
    /// 錯誤的密碼與被修改的備份都以 KRS-E4307 拒絕，且不寫入資料目錄
    #[test]
    fn keys_backup_and_restore_through_the_cli_functions() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-db-keys-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("alerts-secrets.key"), [1u8; 32]).unwrap();
        std::fs::write(data.join("console.key"), [2u8; 32]).unwrap();
        let cfg = dir.join("kairos.toml");
        std::fs::write(
            &cfg,
            format!("[storage]\ndata_dir = {:?}\n", data.to_string_lossy()),
        )
        .unwrap();
        let pass = dir.join("pass.txt");
        std::fs::write(&pass, "correct horse battery staple\n").unwrap();
        let src = PasswordSource::File(pass.clone());
        let dest = dir.join("k.db");
        let (out, names, missing) = backup_keys(Some(&cfg), &dest, &src).unwrap();
        assert_eq!(out, dir.join("k.db.keys"));
        assert_eq!(names, ["alerts-secrets.key", "console.key"]);
        assert_eq!(missing, ["audit-hmac.key", "privacy.key"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        // 已存在的金鑰備份不覆寫
        assert!(backup_keys(Some(&cfg), &dest, &src).is_err());
        // 太短的密碼
        let short = dir.join("short.txt");
        std::fs::write(&short, "hunter2\n").unwrap();
        let e =
            backup_keys(Some(&cfg), &dir.join("x.db"), &PasswordSource::File(short)).unwrap_err();
        assert_eq!(e.code, "KRS-E4307");
        assert!(!dir.join("x.db.keys").exists());

        // 金鑰檔遺失 → 還原
        std::fs::remove_file(data.join("alerts-secrets.key")).unwrap();
        std::fs::remove_file(data.join("console.key")).unwrap();
        // 錯誤的密碼：拒絕，不寫入
        let wrong = dir.join("wrong.txt");
        std::fs::write(&wrong, "incorrect horse battery staple").unwrap();
        let e = restore_keys(Some(&cfg), &out, &PasswordSource::File(wrong), false).unwrap_err();
        assert_eq!(e.code, "KRS-E4307");
        assert!(e.cause.contains("密碼錯誤"), "{}", e.cause);
        assert!(!data.join("console.key").exists());
        // 被修改的備份：拒絕
        let text = std::fs::read_to_string(&out).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut ct = crate::api::http::b64url_decode(v["ciphertext"].as_str().unwrap()).unwrap();
        ct[0] ^= 0x80;
        v["ciphertext"] = serde_json::json!(crate::api::http::b64url_encode(&ct));
        let tampered = dir.join("tampered.keys");
        std::fs::write(&tampered, v.to_string()).unwrap();
        let e = restore_keys(Some(&cfg), &tampered, &src, false).unwrap_err();
        assert_eq!(e.code, "KRS-E4307");
        // 正確的密碼：還原，內容相同、0600
        let done = restore_keys(Some(&cfg), &out, &src, false).unwrap();
        assert_eq!(done.len(), 2);
        assert_eq!(
            std::fs::read(data.join("alerts-secrets.key")).unwrap(),
            [1u8; 32]
        );
        assert_eq!(std::fs::read(data.join("console.key")).unwrap(), [2u8; 32]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for n in ["alerts-secrets.key", "console.key"] {
                let mode = std::fs::metadata(data.join(n))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{n} {mode:o}");
            }
        }
        // 資料目錄已有不同的金鑰：沒有 --overwrite 時拒絕
        std::fs::write(data.join("console.key"), [7u8; 32]).unwrap();
        let e = restore_keys(Some(&cfg), &out, &src, false).unwrap_err();
        assert!(e.cause.contains("--overwrite"), "{}", e.cause);
        assert_eq!(std::fs::read(data.join("console.key")).unwrap(), [7u8; 32]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
