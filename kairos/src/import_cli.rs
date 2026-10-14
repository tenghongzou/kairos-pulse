//! `kairos reports import`：把 0.2.x 以前 Kairos 寫的 JSONL（`reports-YYYY-MM-DD.jsonl`）匯入 SQLite。
//!
//! 本模組負責命令列：參數、來源檔的收集與安全檢查（只讀、不跟隨符號連結）、進度與摘要的輸出、
//! 結束代碼。匯入本身（去重、保留期限、彙總、獨占資料庫）在 [`crate::store::import`]，設計見
//! `kairos/STORAGE.md`「從 JSONL 升級」。
//!
//! 輸出依設計包 `design/design-system/cli-style.md`：進度與每個檔案的結果寫 stderr，摘要（結果）寫
//! stdout；`--json` 時 stdout 只有一份純 ASCII 的 JSON（`schema` 為 `kairos.reports.import/v1`），
//! stderr 保持空白。不輸出顏色，符號依 §4.2 決定 Unicode 或 ASCII。

use chrono::{DateTime, NaiveDate, Utc};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufReader, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::{AppConfig, StorageBackend, take_config_arg};
use crate::hosts_cli::{ascii_json, parse_duration, use_unicode};
use crate::storage::{LEGACY_REPORTS_FILE, REPORT_FILES, fmt_time};
use crate::store::import::{BadLine, Counts, FileReport, ImportError, ImportSettings, Importer};
use crate::store::sqlite::{DB_FILE, SqliteOptions};

/// `kairos reports import --help`
pub const IMPORT_USAGE: &str = "kairos reports import — 把 0.2.x 以前的 JSONL（reports-YYYY-MM-DD.jsonl）匯入 SQLite

用法：
  kairos reports import [選項] <檔案或目錄>...

請先停止 Kairos：匯入期間獨占資料庫，Kairos 執行中時不匯入（結束代碼 11），匯入期間 Kairos 也無法啟動。
--dry-run 只讀取，不需要停止 Kairos。

選項：
  --dry-run          試執行：計算會匯入、重複、壞行與超出保留期的筆數，不寫入資料庫
  --since <時間>     只匯入取樣時間不早於這個時間的資料：2026-09-01（UTC）、2026-09-01T08:00:00+08:00，
                     或 30d、12h（從現在往前）
  --until <時間>     只匯入取樣時間早於這個時間的資料（格式同上）
  --config <path>    Kairos 的設定檔（storage.data_dir 與保留期限；預設為目前目錄的 kairos.toml）
  --json             輸出機器可讀的 JSON（純 ASCII，schema kairos.reports.import/v1）
  --ascii            不使用 Unicode 符號（也可設定環境變數 KAIROS_SYMBOLS=ascii）
  -h, --help         顯示本說明

目錄：匯入其中的 reports-YYYY-MM-DD.jsonl 與舊版的 reports.jsonl（依日期排序，不進入子目錄）。
來源檔只讀取、不修改；符號連結與不是一般檔案的路徑一律不讀取。
同一個檔案重複匯入不會重複寫入（以「主機＋取樣時間＋內容雜湊」去重），中斷後可以直接重新執行。
取樣時間超過 storage.raw_retention_days 的只併入彙總；超過所有彙總保留期限的略過。

範例：
  sudo systemctl stop kairos
  sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --config /opt/kairos/kairos.toml
  sudo systemctl start kairos

結束代碼：0 完成（壞行略過並列在摘要中）、1 匯入失敗或有來源檔無法讀取、2 用法錯誤、3 設定錯誤、
10 權限不足、11 資料庫使用中（Kairos 執行中）";

/// JSON 輸出的 `schema`
pub const JSON_SCHEMA: &str = "kairos.reports.import/v1";

/// 命令本身無法完成（設定、資料庫使用中、無法開啟資料庫、匯入中途的資料庫錯誤）
const CODE_FAILED: &str = "KRS-E4304";
/// 來源檔無法讀取（data.issues[]）
const CODE_SOURCE: &str = "KRS-E4305";
/// 有壞行被略過（data.issues[]）
const CODE_BAD_LINES: &str = "KRS-W4306";
/// 匯入後資料庫超過總容量上限（data.issues[]；與 Kairos 的「容量已滿」相同）
const CODE_CAPACITY: &str = "KRS-W4202";

/// 結束代碼（cli-style.md §8）
const EXIT_GENERAL: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_CONFIG: u8 = 3;
const EXIT_PERMISSION: u8 = 10;
const EXIT_CONFLICT: u8 = 11;

/// 長時間匯入一個檔案時，非 TTY 每這麼久印一行進度（cli-style.md §9.1）
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// `--since`／`--until` 的值：固定時間，或從現在往前
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeArg {
    At(DateTime<Utc>),
    Ago(Duration),
}

impl TimeArg {
    fn parse(text: &str) -> Option<Self> {
        if let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
            return Some(TimeArg::At(d.and_hms_opt(0, 0, 0)?.and_utc()));
        }
        if let Ok(t) = DateTime::parse_from_rfc3339(text) {
            return Some(TimeArg::At(t.with_timezone(&Utc)));
        }
        parse_duration(text).map(TimeArg::Ago)
    }

    fn resolve(self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            TimeArg::At(t) => t,
            TimeArg::Ago(d) => now - chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX),
        }
    }
}

/// `kairos reports import` 的選項
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportOptions {
    pub config_path: Option<PathBuf>,
    pub paths: Vec<PathBuf>,
    pub dry_run: bool,
    pub since: Option<TimeArg>,
    pub until: Option<TimeArg>,
    pub json: bool,
    pub ascii: bool,
}

/// 解析 `kairos reports import` 之後的參數；`Ok(None)` 為 `--help`
pub fn parse_args(
    args: &mut std::iter::Peekable<impl Iterator<Item = OsString>>,
) -> Result<Option<ImportOptions>, String> {
    let mut opts = ImportOptions::default();
    let mut only_paths = false;
    while let Some(arg) = args.next() {
        if only_paths {
            opts.paths.push(PathBuf::from(arg));
            continue;
        }
        if let Some(path) = take_config_arg(&arg, args)? {
            if opts.config_path.replace(path).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        let Some(a) = arg.to_str().map(str::to_string) else {
            // 不是 UTF-8 的參數只可能是路徑
            opts.paths.push(PathBuf::from(arg));
            continue;
        };
        if !a.starts_with('-') || a == "-" {
            opts.paths.push(PathBuf::from(arg));
            continue;
        }
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
            "-h" | "--help" => return Ok(None),
            "--" => only_paths = true,
            "--dry-run" => opts.dry_run = true,
            "--json" => opts.json = true,
            "--ascii" => opts.ascii = true,
            "--since" | "--until" => {
                let v = value()?;
                let t = TimeArg::parse(&v).ok_or_else(|| {
                    format!("{name} 的值 \"{v}\" 無效，例如 2026-09-01、2026-09-01T08:00:00Z、30d")
                })?;
                let slot = if name == "--since" {
                    &mut opts.since
                } else {
                    &mut opts.until
                };
                if slot.replace(t).is_some() {
                    return Err(format!("{name} 只能指定一次"));
                }
            }
            _ => return Err(format!("無法辨識的參數: {a}")),
        }
    }
    if opts.paths.is_empty() {
        return Err("kairos reports import 需要至少一個檔案或目錄".to_string());
    }
    Ok(Some(opts))
}

/// 命令本身無法完成時的錯誤
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    code: &'static str,
    title: String,
    cause: String,
    fix: Vec<String>,
    exit: u8,
}

/// 一個要匯入的來源檔或無法使用的路徑
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProblem {
    pub path: PathBuf,
    pub reason: String,
    pub permission: bool,
}

/// 收集來源檔：檔案照原樣；目錄取其中的 `reports-YYYY-MM-DD.jsonl` 與 `reports.jsonl`（不進入子目錄、
/// 不跟隨符號連結）。回傳（依序要匯入的檔案, 無法使用的路徑, 給使用者的提示）
pub fn collect_sources(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<SourceProblem>, Vec<String>) {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    let mut notes = Vec::new();
    for path in paths {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                problems.push(SourceProblem {
                    path: path.clone(),
                    reason: match e.kind() {
                        ErrorKind::NotFound => "找不到這個檔案或目錄".to_string(),
                        _ => format!("無法讀取：{e}"),
                    },
                    permission: e.kind() == ErrorKind::PermissionDenied,
                });
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            problems.push(SourceProblem {
                path: path.clone(),
                reason: "是符號連結，不跟隨（請指定實際的檔案或目錄）".to_string(),
                permission: false,
            });
            continue;
        }
        if meta.is_file() {
            files.push(path.clone());
            continue;
        }
        if !meta.is_dir() {
            problems.push(SourceProblem {
                path: path.clone(),
                reason: "不是一般檔案或目錄".to_string(),
                permission: false,
            });
            continue;
        }
        let entries = match std::fs::read_dir(path) {
            Ok(e) => e,
            Err(e) => {
                problems.push(SourceProblem {
                    path: path.clone(),
                    reason: format!("無法列出目錄：{e}"),
                    permission: e.kind() == ErrorKind::PermissionDenied,
                });
                continue;
            }
        };
        let mut found: Vec<(Option<NaiveDate>, PathBuf)> = Vec::new();
        let (mut links, mut compressed) = (0, 0);
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let date = REPORT_FILES.parse(name);
            if date.is_none() && name != LEGACY_REPORTS_FILE {
                if name.starts_with(REPORT_FILES.prefix)
                    && [".gz", ".zst", ".xz", ".bz2"]
                        .iter()
                        .any(|s| name.ends_with(s))
                {
                    compressed += 1;
                }
                continue;
            }
            // file_type 不跟隨符號連結
            match entry.file_type() {
                Ok(t) if t.is_symlink() => links += 1,
                Ok(t) if t.is_file() => found.push((date, entry.path())),
                _ => {}
            }
        }
        // 舊版的 reports.jsonl（日期為 None）最先，其餘依日期
        found.sort();
        if found.is_empty() {
            notes.push(format!(
                "{} 中沒有 reports-YYYY-MM-DD.jsonl",
                path.display()
            ));
        }
        if links > 0 {
            notes.push(format!(
                "{} 中有 {links} 個符號連結，未讀取",
                path.display()
            ));
        }
        if compressed > 0 {
            notes.push(format!(
                "{} 中有 {compressed} 個壓縮過的 report 檔，未讀取（Kairos 不會產生壓縮檔；請先解壓縮成 reports-YYYY-MM-DD.jsonl 再匯入）",
                path.display()
            ));
        }
        files.extend(found.into_iter().map(|(_, p)| p));
    }
    (files, problems, notes)
}

/// 以唯讀方式開啟來源檔：拒絕符號連結與不是一般檔案的路徑。Unix 上另比對開啟前後的裝置與 inode，
/// 檢查與開啟之間被換成符號連結或其他檔案時拒絕（比照 `kairos db backup` 不跟隨預先放置的連結）
pub fn open_source(path: &Path) -> std::io::Result<File> {
    let before = std::fs::symlink_metadata(path)?;
    if before.file_type().is_symlink() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "是符號連結，不跟隨（請指定實際的檔案）",
        ));
    }
    if !before.is_file() {
        return Err(std::io::Error::new(ErrorKind::InvalidInput, "不是一般檔案"));
    }
    let file = File::open(path)?;
    let after = file.metadata()?;
    if !after.is_file() {
        return Err(std::io::Error::new(ErrorKind::InvalidInput, "不是一般檔案"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "檢查後檔案被換掉（可能是符號連結），未讀取",
            ));
        }
    }
    Ok(file)
}

/// 一個來源檔的結果
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileOutcome {
    path: PathBuf,
    report: Option<FileReport>,
    error: Option<SourceProblem>,
}

/// 千分位（給人看的數字）
fn num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// 輸出前跳脫控制字元（路徑、壞行的原因可能含有來自檔案的字元）
fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_unicode().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn bad_samples_text(samples: &[BadLine]) -> String {
    samples
        .iter()
        .map(|b| format!("第 {} 行（{}）", b.line, escape(&b.reason)))
        .collect::<Vec<_>>()
        .join("、")
}

/// 一個檔案完成時的一行（stderr）
fn file_line(path: &Path, r: &FileReport, unicode: bool, dry_run: bool) -> String {
    let c = &r.counts;
    let mark = match (c.bad_lines > 0, unicode) {
        (false, true) => "✓",
        (false, false) => "[ OK ]",
        (true, true) => "⚠",
        (true, false) => "[WARN]",
    };
    let verb = if dry_run { "將匯入" } else { "匯入" };
    let mut parts = vec![format!("{verb} {}", num(c.imported()))];
    for (n, label) in [
        (c.duplicates, "重複"),
        (c.bad_lines, "壞行"),
        (c.expired, "超出保留期"),
        (c.out_of_range, "不在範圍"),
        (c.rejected, "無法匯入"),
    ] {
        if n > 0 {
            parts.push(format!("{label} {}", num(n)));
        }
    }
    let mut out = format!(
        "{mark} {}：{} 行，{}\n",
        escape(&file_name(path)),
        num(c.lines_read),
        parts.join("、")
    );
    if !r.bad_samples.is_empty() {
        out.push_str(&format!("  壞行：{}\n", bad_samples_text(&r.bad_samples)));
    }
    out
}

/// 摘要（stdout）
#[allow(clippy::too_many_arguments)]
fn render_summary(
    outcomes: &[FileOutcome],
    totals: &Counts,
    new_hosts: u64,
    db: &Path,
    opts: &ImportOptions,
    notes: &[String],
    over_capacity: Option<(u64, u64)>,
    unicode: bool,
) -> String {
    let imported_files = outcomes.iter().filter(|o| o.report.is_some()).count();
    let failed: Vec<&SourceProblem> = outcomes.iter().filter_map(|o| o.error.as_ref()).collect();
    let mut out = String::new();
    let (ok, warn, fail, info) = if unicode {
        ("✓", "⚠", "✗", "i")
    } else {
        ("[ OK ]", "[WARN]", "[FAIL]", "[INFO]")
    };
    if opts.dry_run {
        out.push_str(&format!(
            "{info} 試執行（沒有寫入資料庫）：讀取 {imported_files} 個檔案，對照 {}\n",
            escape(&db.display().to_string())
        ));
    } else {
        out.push_str(&format!(
            "{ok} 已匯入 {imported_files} 個檔案到 {}\n",
            escape(&db.display().to_string())
        ));
    }
    let verb = if opts.dry_run { "將匯入" } else { "匯入" };
    let mut rows: Vec<(String, String)> = vec![
        ("讀取".into(), format!("{} 行", num(totals.lines_read))),
        (
            verb.into(),
            format!(
                "{} 筆（原始資料 {}、只進彙總 {}）",
                num(totals.imported()),
                num(totals.imported_raw),
                num(totals.imported_rollup_only)
            ),
        ),
        ("重複略過".into(), num(totals.duplicates)),
        ("壞行".into(), num(totals.bad_lines)),
        ("超出保留期".into(), num(totals.expired)),
    ];
    if opts.since.is_some() || opts.until.is_some() || totals.out_of_range > 0 {
        rows.push(("不在範圍".into(), num(totals.out_of_range)));
    }
    if totals.rejected > 0 {
        rows.push((
            "無法匯入".into(),
            format!("{}（主機清冊或指標序列數已達上限）", num(totals.rejected)),
        ));
    }
    rows.push((
        if opts.dry_run {
            "將新增主機"
        } else {
            "新增主機"
        }
        .into(),
        format!("{} 台", num(new_hosts)),
    ));
    let width = rows
        .iter()
        .map(|(k, _)| crate::hosts_cli::display_width(k))
        .max()
        .unwrap_or(0);
    for (k, v) in &rows {
        out.push_str(&format!(
            "  {k}{}  {v}\n",
            " ".repeat(width - crate::hosts_cli::display_width(k))
        ));
    }
    if totals.bad_lines > 0 {
        out.push_str(&format!(
            "{warn} 警告 {CODE_BAD_LINES}：{} 行不是合法的 report，已略過\n  處理方式：上方列出各檔案前幾個壞行的行號與原因；確認是否為截斷或手動編輯過的檔案。修正後重新執行，已匯入的部分不會重複。\n",
            num(totals.bad_lines)
        ));
    }
    if let Some((used, cap)) = over_capacity {
        out.push_str(&format!(
            "{warn} 警告 {CODE_CAPACITY}：資料庫使用 {} bytes，超過 storage.max_total_bytes（{} bytes）\n  處理方式：Kairos 啟動後會依寫入順序刪除最舊的原始 report，匯入的資料最後寫入、會最晚被刪；請調高 storage.max_total_bytes，或以 --since 縮小匯入範圍。\n",
            num(used),
            num(cap)
        ));
    }
    if !failed.is_empty() {
        out.push_str(&format!(
            "{fail} 錯誤 {CODE_SOURCE}：{} 個來源無法讀取，未匯入\n",
            failed.len()
        ));
        for p in &failed {
            out.push_str(&format!(
                "  • {}：{}\n",
                escape(&p.path.display().to_string()),
                escape(&p.reason)
            ));
        }
        out.push_str("  處理方式：確認路徑與權限（以 Kairos 的服務帳號執行，例如 sudo -u kairos），修正後重新執行。\n");
    }
    for n in notes {
        out.push_str(&format!("{info} {}\n", escape(n)));
    }
    if !unicode {
        out = out.replace('•', "*");
    }
    out
}

/// JSON 的計數
fn counts_json(c: &Counts) -> serde_json::Value {
    serde_json::json!({
        "lines_read": c.lines_read,
        "imported": c.imported(),
        "imported_raw": c.imported_raw,
        "imported_rollup_only": c.imported_rollup_only,
        "duplicates": c.duplicates,
        "bad_lines": c.bad_lines,
        "expired": c.expired,
        "out_of_range": c.out_of_range,
        "rejected": c.rejected,
    })
}

fn issue(
    code: &str,
    severity: &str,
    title: &str,
    cause: String,
    fix: &str,
    details: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "code": code,
        "severity": severity,
        "title": title,
        "cause": cause,
        "fix": [fix],
        "details": details,
        "explain": null,
    })
}

#[allow(clippy::too_many_arguments)]
fn render_json(
    now: DateTime<Utc>,
    outcomes: &[FileOutcome],
    totals: &Counts,
    new_hosts: u64,
    db: &Path,
    opts: &ImportOptions,
    range: (Option<DateTime<Utc>>, Option<DateTime<Utc>>),
    used_bytes: Option<u64>,
    over_capacity: Option<(u64, u64)>,
    notes: &[String],
) -> String {
    let mut issues = Vec::new();
    let files: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|o| {
            if let Some(p) = &o.error {
                issues.push(issue(
                    CODE_SOURCE,
                    "error",
                    "無法讀取來源檔",
                    p.reason.clone(),
                    "確認路徑與權限（以 Kairos 的服務帳號執行），修正後重新執行。",
                    serde_json::json!({ "path": p.path.display().to_string() }),
                ));
            }
            match &o.report {
                Some(r) => {
                    if r.counts.bad_lines > 0 {
                        issues.push(issue(
                            CODE_BAD_LINES,
                            "warning",
                            "略過不合法的行",
                            format!("{} 行不是合法的 report", r.counts.bad_lines),
                            "確認是否為截斷或手動編輯過的檔案；修正後重新執行，已匯入的部分不會重複。",
                            serde_json::json!({
                                "path": o.path.display().to_string(),
                                "samples": r.bad_samples,
                            }),
                        ));
                    }
                    serde_json::json!({
                        "path": o.path.display().to_string(),
                        "status": "read",
                        "counts": counts_json(&r.counts),
                        "bad_line_samples": r.bad_samples,
                        "incomplete_tail": r.incomplete_tail,
                    })
                }
                None => serde_json::json!({
                    "path": o.path.display().to_string(),
                    "status": "failed",
                    "counts": null,
                    "bad_line_samples": [],
                    "incomplete_tail": false,
                }),
            }
        })
        .collect();
    if let Some((used, cap)) = over_capacity {
        issues.push(issue(
            CODE_CAPACITY,
            "warning",
            "資料庫超過總容量上限",
            format!("資料庫使用 {used} bytes，超過 storage.max_total_bytes（{cap} bytes）"),
            "調高 storage.max_total_bytes，或以 --since 縮小匯入範圍；否則 Kairos 啟動後會刪除最舊的原始 report。",
            serde_json::json!({ "used_bytes": used, "max_total_bytes": cap }),
        ));
    }
    let doc = serde_json::json!({
        "schema": JSON_SCHEMA,
        "ok": true,
        "generated_at": fmt_time(now),
        "data": {
            "dry_run": opts.dry_run,
            "database": db.display().to_string(),
            "since": range.0.map(fmt_time),
            "until": range.1.map(fmt_time),
            "files": files,
            "totals": counts_json(totals),
            "new_hosts": new_hosts,
            "used_bytes": used_bytes,
            "notes": notes,
            "issues": issues,
        },
        "warnings": [],
        "error": null,
    });
    ascii_json(&doc) + "\n"
}

fn render_failure_json(now: DateTime<Utc>, f: &Failure) -> String {
    let doc = serde_json::json!({
        "schema": JSON_SCHEMA,
        "ok": false,
        "generated_at": fmt_time(now),
        "data": null,
        "warnings": [],
        "error": {
            "code": f.code,
            "severity": "error",
            "title": f.title,
            "cause": f.cause,
            "fix": f.fix,
            "details": {},
            "explain": null,
        },
    });
    ascii_json(&doc) + "\n"
}

fn render_failure_human(f: &Failure, unicode: bool) -> String {
    let mark = if unicode { "✗" } else { "[FAIL]" };
    let mut out = format!(
        "{mark} 錯誤 {}：{}\n  原因：{}\n  處理方式：\n",
        f.code,
        f.title,
        escape(&f.cause)
    );
    for (i, fix) in f.fix.iter().enumerate() {
        out.push_str(&format!("    {}. {}\n", i + 1, fix));
    }
    out
}

const TITLE: &str = "無法匯入 report";

fn failure(cause: String, fix: Vec<String>, exit: u8) -> Failure {
    Failure {
        code: CODE_FAILED,
        title: TITLE.to_string(),
        cause,
        fix,
        exit,
    }
}

/// 開啟資料庫失敗、匯入中途資料庫錯誤時的結束代碼與處理方式
fn db_failure(e: &std::io::Error, dry_run: bool) -> Failure {
    match e.kind() {
        ErrorKind::ResourceBusy => failure(
            e.to_string(),
            vec![
                "先停止 Kairos 再匯入：sudo systemctl stop kairos；匯入完成後再 sudo systemctl start kairos。".to_string(),
                "也可以先以 --dry-run 確認要匯入的筆數（不需要停止 Kairos）。".to_string(),
            ],
            EXIT_CONFLICT,
        ),
        ErrorKind::PermissionDenied => failure(
            e.to_string(),
            vec![format!(
                "以 Kairos 的服務帳號執行，例如：sudo -u kairos /opt/kairos/kairos reports import{} <路徑> --config /opt/kairos/kairos.toml",
                if dry_run { " --dry-run" } else { "" }
            )],
            EXIT_PERMISSION,
        ),
        _ => failure(
            e.to_string(),
            vec![
                "確認 storage.data_dir 指向 Kairos 的資料目錄、磁碟有足夠空間；資料庫損毀時請從備份還原（kairos/STORAGE.md「備份與還原」）。".to_string(),
                "已提交的部分不會遺失；修正後重新執行同一個命令即可（已匯入的會被略過）。".to_string(),
            ],
            EXIT_GENERAL,
        ),
    }
}

/// root 執行時拒絕寫入：新建立的資料庫、`-wal` 會屬於 root，之後以服務帳號執行的 Kairos 無法開啟
fn refuse_root(data_dir: &Path) -> Option<Failure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if crate::fsperm::effective_uid() != Some(0) {
            return None;
        }
        // 資料目錄還不存在時（第一次啟動 0.3 之前匯入）看最近的上層目錄
        let mut dir = data_dir.to_path_buf();
        let owner = loop {
            let probe = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir.as_path()
            };
            match std::fs::metadata(probe) {
                Ok(m) => break m.uid(),
                Err(_) if dir.pop() => {}
                Err(_) => return None,
            }
        };
        (owner != 0).then(|| {
            failure(
                format!(
                    "以 root 執行，但資料目錄 {} 屬於 uid {owner}：匯入時建立的資料庫檔會屬於 root，Kairos（服務帳號）之後無法寫入",
                    data_dir.display()
                ),
                vec!["以資料目錄的擁有者（Kairos 的服務帳號）執行，例如：sudo -u kairos /opt/kairos/kairos reports import <路徑> --config /opt/kairos/kairos.toml".to_string()],
                EXIT_PERMISSION,
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        None
    }
}

/// 執行 `kairos reports import`，回傳結束代碼
pub fn run(opts: ImportOptions) -> u8 {
    let now = Utc::now();
    let unicode = !opts.json && use_unicode(opts.ascii);
    let result = run_inner(&opts, now, unicode);
    let (text, code) = match result {
        Ok((text, code)) => (text, code),
        Err(f) => {
            if opts.json {
                (render_failure_json(now, &f), f.exit)
            } else {
                eprint!("{}", render_failure_human(&f, use_unicode(opts.ascii)));
                (String::new(), f.exit)
            }
        }
    };
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|_| stdout.flush())
    {
        Ok(()) => code,
        // 輸出被關閉（例如 | head）時，匯入本身已完成，結束代碼不變
        Err(e) if e.kind() == ErrorKind::BrokenPipe => code,
        Err(_) => code.max(EXIT_GENERAL),
    }
}

fn run_inner(
    opts: &ImportOptions,
    now: DateTime<Utc>,
    unicode: bool,
) -> Result<(String, u8), Failure> {
    let config = AppConfig::load(opts.config_path.as_deref()).map_err(|e| Failure {
        code: crate::diag::config_load_code(&e, opts.config_path.as_deref()),
        title: "設定檔無法載入".to_string(),
        cause: e.to_string(),
        fix: vec!["修正設定檔後再執行，或以 --config 指定 Kairos 使用的設定檔。".to_string()],
        exit: EXIT_CONFIG,
    })?;
    if config.storage.backend != StorageBackend::Sqlite {
        return Err(failure(
            "storage.backend 為 \"jsonl\"，沒有 SQLite 資料庫。".to_string(),
            vec!["匯入需要 storage.backend = \"sqlite\"（預設值）。".to_string()],
            EXIT_CONFIG,
        ));
    }
    let data_dir = Path::new(&config.storage.data_dir);
    let sqlite = SqliteOptions::from_config(&config, data_dir);
    let db = data_dir.join(DB_FILE);
    let since = opts.since.map(|t| t.resolve(now));
    let until = opts.until.map(|t| t.resolve(now));
    if let (Some(s), Some(u)) = (since, until)
        && s >= u
    {
        return Err(Failure {
            code: CODE_FAILED,
            title: TITLE.to_string(),
            cause: format!(
                "--since（{}）不早於 --until（{}）",
                fmt_time(s),
                fmt_time(u)
            ),
            fix: vec!["調整時間範圍，讓 --since 早於 --until。".to_string()],
            exit: EXIT_USAGE,
        });
    }
    if !opts.dry_run
        && let Some(f) = refuse_root(data_dir)
    {
        return Err(f);
    }
    let (files, problems, notes) = collect_sources(&opts.paths);
    if files.is_empty() && problems.is_empty() {
        return Err(failure(
            notes.join("；"),
            vec!["指定 Kairos 0.2.x 的資料目錄（例如 /opt/kairos/data）或其中的 reports-YYYY-MM-DD.jsonl。".to_string()],
            EXIT_GENERAL,
        ));
    }
    let settings = ImportSettings {
        dry_run: opts.dry_run,
        since_ms: since.map(|t| t.timestamp_millis()),
        until_ms: until.map(|t| t.timestamp_millis()),
        now,
    };
    let mut importer =
        Importer::open(&sqlite, settings).map_err(|e| db_failure(&e, opts.dry_run))?;
    let progress_out = !opts.json;
    if progress_out {
        eprintln!(
            "{} · 設定：{} · 資料庫：{}{}",
            crate::VERSION,
            escape(
                &opts
                    .config_path
                    .as_deref()
                    .unwrap_or(Path::new("kairos.toml"))
                    .display()
                    .to_string()
            ),
            escape(&db.display().to_string()),
            if opts.dry_run {
                "（試執行，唯讀）"
            } else {
                ""
            }
        );
    }
    let mut outcomes: Vec<FileOutcome> = problems
        .into_iter()
        .map(|p| FileOutcome {
            path: p.path.clone(),
            report: None,
            error: Some(p),
        })
        .collect();
    if progress_out {
        for o in &outcomes {
            if let Some(p) = &o.error {
                eprintln!(
                    "{} {}：{}",
                    if unicode { "✗" } else { "[FAIL]" },
                    escape(&p.path.display().to_string()),
                    escape(&p.reason)
                );
            }
        }
    }
    let mut totals = Counts::default();
    let tty = std::io::stderr().is_terminal();
    for path in files {
        let file = match open_source(&path) {
            Ok(f) => f,
            Err(e) => {
                let p = SourceProblem {
                    path: path.clone(),
                    reason: format!("無法開啟：{e}"),
                    permission: e.kind() == ErrorKind::PermissionDenied,
                };
                if progress_out {
                    eprintln!(
                        "{} {}：{}",
                        if unicode { "✗" } else { "[FAIL]" },
                        escape(&path.display().to_string()),
                        escape(&p.reason)
                    );
                }
                outcomes.push(FileOutcome {
                    path,
                    report: None,
                    error: Some(p),
                });
                continue;
            }
        };
        let started = Instant::now();
        let mut last = Instant::now();
        let name = escape(&file_name(&path));
        let mut progress = |c: &Counts| {
            if progress_out && last.elapsed() >= PROGRESS_INTERVAL {
                last = Instant::now();
                let mark = if unicode { "…" } else { "[WAIT]" };
                if tty {
                    eprintln!("{mark} {name}：已讀取 {} 行", num(c.lines_read));
                } else {
                    eprintln!(
                        "{mark} {name}：已讀取 {} 行（{} 秒）",
                        num(c.lines_read),
                        started.elapsed().as_secs()
                    );
                }
            }
        };
        let reader = BufReader::with_capacity(256 * 1024, file);
        match importer.import_reader(reader, &mut progress) {
            Ok(report) => {
                if progress_out {
                    eprint!("{}", file_line(&path, &report, unicode, opts.dry_run));
                }
                totals.add(&report.counts);
                outcomes.push(FileOutcome {
                    path,
                    report: Some(report),
                    error: None,
                });
            }
            Err(ImportError::Read(e)) => {
                let p = SourceProblem {
                    path: path.clone(),
                    reason: format!("讀取中途失敗（之前的部分已匯入）：{e}"),
                    permission: e.kind() == ErrorKind::PermissionDenied,
                };
                if progress_out {
                    eprintln!(
                        "{} {}：{}",
                        if unicode { "✗" } else { "[FAIL]" },
                        escape(&path.display().to_string()),
                        escape(&p.reason)
                    );
                }
                outcomes.push(FileOutcome {
                    path,
                    report: None,
                    error: Some(p),
                });
            }
            Err(ImportError::Db(e)) => return Err(db_failure(&e, opts.dry_run)),
        }
    }
    let finished = importer
        .finish()
        .map_err(|e| db_failure(&e, opts.dry_run))?;
    let over = finished
        .over_capacity()
        .then(|| (finished.used_bytes.unwrap_or(0), finished.max_total_bytes));
    let text = if opts.json {
        render_json(
            now,
            &outcomes,
            &totals,
            finished.new_hosts,
            &db,
            opts,
            (since, until),
            finished.used_bytes,
            over,
            &notes,
        )
    } else {
        render_summary(
            &outcomes,
            &totals,
            finished.new_hosts,
            &db,
            opts,
            &notes,
            over,
            unicode,
        )
    };
    let code = outcomes
        .iter()
        .filter_map(|o| o.error.as_ref())
        .map(|p| {
            if p.permission {
                EXIT_PERMISSION
            } else {
                EXIT_GENERAL
            }
        })
        .max()
        .unwrap_or(0);
    Ok((text, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::next_conn_id;
    use crate::storage::{RecordBody, ReportRecord};

    fn os(list: &[&str]) -> std::iter::Peekable<std::vec::IntoIter<OsString>> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
            .peekable()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kairos-import-cli-{}-{}-{}",
            tag,
            std::process::id(),
            next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn argument_parsing() {
        assert_eq!(parse_args(&mut os(&["--help"])), Ok(None));
        assert_eq!(
            parse_args(&mut os(&[
                "data",
                "--dry-run",
                "--since=2026-09-01",
                "--until",
                "12h",
                "--config",
                "k.toml",
                "--json",
                "--",
                "--odd-name.jsonl",
            ])),
            Ok(Some(ImportOptions {
                config_path: Some(PathBuf::from("k.toml")),
                paths: vec![PathBuf::from("data"), PathBuf::from("--odd-name.jsonl")],
                dry_run: true,
                since: Some(TimeArg::At(
                    NaiveDate::from_ymd_opt(2026, 9, 1)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap()
                        .and_utc()
                )),
                until: Some(TimeArg::Ago(Duration::from_secs(12 * 3600))),
                json: true,
                ascii: false,
            }))
        );
        for bad in [
            &[][..],
            &["--dry-run"],
            &["data", "--since"],
            &["data", "--since", "yesterday"],
            &["data", "--since", "1d", "--since", "2d"],
            &["data", "--force"],
        ] {
            assert!(parse_args(&mut os(bad)).is_err(), "{bad:?}");
        }
        assert!(matches!(
            crate::config::parse_args(os(&["reports", "import", "data"])),
            Ok(crate::config::CliAction::Reports(
                crate::export_cli::ReportsCommand::Import(_)
            ))
        ));
        assert!(matches!(
            crate::config::parse_args(os(&["reports", "import", "-h"])),
            Ok(crate::config::CliAction::Reports(
                crate::export_cli::ReportsCommand::ImportHelp
            ))
        ));
    }

    #[test]
    fn time_arguments() {
        let now = DateTime::parse_from_rfc3339("2026-10-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = |s: &str| TimeArg::parse(s).map(|a| fmt_time(a.resolve(now)));
        assert_eq!(t("2026-09-01").as_deref(), Some("2026-09-01T00:00:00.000Z"));
        assert_eq!(
            t("2026-09-01T08:00:00+08:00").as_deref(),
            Some("2026-09-01T00:00:00.000Z")
        );
        assert_eq!(t("1d12h").as_deref(), Some("2026-10-08T00:00:00.000Z"));
        assert_eq!(t("2026-13-01"), None);
        assert_eq!(t("soon"), None);
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(num(0), "0");
        assert_eq!(num(999), "999");
        assert_eq!(num(1000), "1,000");
        assert_eq!(num(1_234_567), "1,234,567");
    }

    #[test]
    fn directories_are_scanned_in_date_order_without_following_links() {
        let dir = temp_dir("collect");
        for name in [
            "reports-2026-10-02.jsonl",
            "reports-2026-09-30.jsonl",
            "reports.jsonl",
            "reports-2026-10-01.jsonl.gz",
            "reports-2026-1-01.jsonl",
            "agents.json",
            "kairos.db",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        std::fs::create_dir(dir.join("reports-2026-10-03.jsonl")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", dir.join("reports-2026-10-04.jsonl")).unwrap();
        let (files, problems, notes) = collect_sources(&[dir.clone(), dir.join("missing.jsonl")]);
        let names: Vec<String> = files.iter().map(|p| file_name(p)).collect();
        assert_eq!(
            names,
            [
                "reports.jsonl",
                "reports-2026-09-30.jsonl",
                "reports-2026-10-02.jsonl"
            ]
        );
        assert_eq!(problems.len(), 1);
        assert!(problems[0].reason.contains("找不到"));
        assert!(
            notes.iter().any(|n| n.contains("1 個壓縮過的")),
            "{notes:?}"
        );
        #[cfg(unix)]
        {
            assert!(
                notes.iter().any(|n| n.contains("1 個符號連結")),
                "{notes:?}"
            );
            // 直接指定符號連結：不跟隨
            let (files, problems, _) = collect_sources(&[dir.join("reports-2026-10-04.jsonl")]);
            assert!(files.is_empty());
            assert!(problems[0].reason.contains("符號連結"));
            let err = open_source(&dir.join("reports-2026-10-04.jsonl")).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
        }
        assert!(open_source(&dir.join("reports-2026-10-03.jsonl")).is_err());
        assert!(open_source(&dir.join("reports.jsonl")).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn report_line(agent: &str, seq: u64, recv_at: DateTime<Utc>) -> String {
        serde_json::to_string(&ReportRecord::new(
            recv_at,
            "10.0.0.1:5000".into(),
            agent.into(),
            "web-01".into(),
            seq,
            fmt_time(recv_at),
            RecordBody::Legacy {
                cpu_usage: 1.0,
                total_memory: 4096,
                used_memory: 1024,
            },
        ))
        .unwrap()
    }

    #[test]
    fn run_imports_once_and_reports_exit_codes() {
        let dir = temp_dir("run");
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let config = dir.join("kairos.toml");
        std::fs::write(
            &config,
            format!("[storage]\ndata_dir = {:?}\n", data.display().to_string()),
        )
        .unwrap();
        let day = Utc::now() - chrono::Duration::days(1);
        let agent = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
        let file = data.join(format!("reports-{}.jsonl", day.format("%Y-%m-%d")));
        std::fs::write(
            &file,
            format!(
                "{}\n{}\nnot json\n",
                report_line(agent, 1, day),
                report_line(agent, 2, day + chrono::Duration::seconds(5))
            ),
        )
        .unwrap();
        let opts = |paths: Vec<PathBuf>| ImportOptions {
            config_path: Some(config.clone()),
            paths,
            ascii: true,
            ..ImportOptions::default()
        };
        let count = || -> i64 {
            crate::store::sqlite::open_read_only(&data.join(DB_FILE))
                .unwrap()
                .query_row("SELECT count(*) FROM reports", [], |r| r.get(0))
                .unwrap()
        };
        // 試執行不建立資料庫
        assert_eq!(
            run(ImportOptions {
                dry_run: true,
                ..opts(vec![data.clone()])
            }),
            0
        );
        assert!(!data.join(DB_FILE).exists());
        // 匯入（壞行不影響結束代碼），重複執行不重複寫入
        assert_eq!(run(opts(vec![data.clone()])), 0);
        assert_eq!(count(), 2);
        assert_eq!(
            run(ImportOptions {
                json: true,
                ..opts(vec![file.clone()])
            }),
            0
        );
        assert_eq!(count(), 2);
        // 有來源無法讀取：其餘照常匯入，結束代碼 1
        assert_eq!(run(opts(vec![file.clone(), dir.join("missing")])), 1);
        // --since 不早於 --until：用法錯誤
        assert_eq!(
            run(ImportOptions {
                since: TimeArg::parse("1d"),
                until: TimeArg::parse("2d"),
                ..opts(vec![file.clone()])
            }),
            2
        );
        // 設定檔有誤：結束代碼 3
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[storage\n").unwrap();
        assert_eq!(
            run(ImportOptions {
                config_path: Some(bad),
                ..opts(vec![file.clone()])
            }),
            EXIT_CONFIG
        );
        // Kairos 執行中：結束代碼 11
        let store_opts = SqliteOptions {
            path: data.join(DB_FILE),
            synchronous: crate::config::SqliteSynchronous::Normal,
            raw_retention_days: 7,
            rollup_5m_retention_days: 31,
            rollup_1h_retention_days: 400,
            max_total_bytes: 0,
            max_hosts: 1000,
            mtls: false,
        };
        let (store, _) = crate::store::sqlite::SqliteStore::open(store_opts, Utc::now).unwrap();
        assert_eq!(run(opts(vec![file.clone()])), EXIT_CONFLICT);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
