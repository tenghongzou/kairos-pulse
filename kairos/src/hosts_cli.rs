//! `kairos hosts list`：查詢 SQLite 主機清冊（審計 A6），回答「哪些主機已失聯 N 天」。
//!
//! 以唯讀方式開啟 `<storage.data_dir>/kairos.db`，Kairos 執行中也可以查詢（WAL）。輸出依設計包
//! `design/design-system/cli-style.md`：結果在 stdout、錯誤在 stderr；`--json` 為純 ASCII 的單一
//! JSON 文件（`schema` 為 `kairos.hosts.list/v1`，錯誤也以 JSON 寫到 stdout）。不輸出顏色（每個
//! 狀態都有文字），符號依 §4.2 決定 Unicode 或 ASCII。

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::ffi::OsString;
use std::io::{ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{AppConfig, StorageBackend, take_config_arg};
use crate::storage::fmt_time;
use crate::store::INVENTORY_SYNC_INTERVAL;
use crate::store::sqlite::{DB_FILE, HostRow, list_hosts, open_read_only};

/// `kairos hosts --help`
pub const HOSTS_USAGE: &str = "kairos hosts list — 列出主機清冊（SQLite）

用法：
  kairos hosts list [選項]

選項：
  --offline-for <時間>  只列出失聯（離線或狀態未知）至少這麼久的主機，例如 90m、12h、3d、2w
  --config <path>       Kairos 的設定檔（用來找到 storage.data_dir；預設為目前目錄的 kairos.toml）
  --wide                顯示完整的 agent_id 與更多欄位
  --no-header           不印表頭與摘要（腳本請改用 --json）
  --json                輸出機器可讀的 JSON（純 ASCII，schema kairos.hosts.list/v1）
  --ascii               不使用 Unicode 符號（也可設定環境變數 KAIROS_SYMBOLS=ascii）
  -h, --help            顯示本說明

範例：
  kairos hosts list --config /opt/kairos/kairos.toml
  kairos hosts list --offline-for 3d
  kairos hosts list --offline-for 3d --json > offline.json

狀態：線上、離線，以及「未知」（清冊記錄為線上，但已超過離線門檻沒有更新，例如 Kairos 未正常關機）。
清冊每 30 秒從 Kairos 的狀態表同步一次，最後訊息的時間最多延遲約 90 秒。

結束代碼：0 正常、1 無法讀取資料庫、2 用法錯誤、3 設定錯誤、10 權限不足";

/// JSON 輸出的 `schema`
pub const JSON_SCHEMA: &str = "kairos.hosts.list/v1";

/// `kairos hosts` 的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostsCommand {
    Help,
    List(ListOptions),
}

/// `kairos hosts list` 的選項
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListOptions {
    pub config_path: Option<PathBuf>,
    pub offline_for: Option<Duration>,
    pub json: bool,
    pub ascii: bool,
    pub wide: bool,
    pub no_header: bool,
}

/// 解析 `90m`、`12h`、`3d`、`2w`、`1d12h` 等時間長度（單位 s、m、h、d、w，可組合）
pub fn parse_duration(text: &str) -> Option<Duration> {
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut any = false;
    for c in text.trim().chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit: u64 = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 7 * 86_400,
            _ => return None,
        };
        let n: u64 = digits.parse().ok()?;
        digits.clear();
        total = total.checked_add(n.checked_mul(unit)?)?;
        any = true;
    }
    (any && digits.is_empty()).then(|| Duration::from_secs(total))
}

/// 解析 `kairos hosts` 之後的參數
pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<HostsCommand, String> {
    let mut args = args.peekable();
    match args.next() {
        None => return Err("kairos hosts 需要子命令（目前只有 list）".to_string()),
        Some(a) if a == "-h" || a == "--help" => return Ok(HostsCommand::Help),
        Some(a) if a == "list" => {}
        Some(a) => {
            return Err(format!(
                "無法辨識的子命令: {}（目前只有 list）",
                a.to_string_lossy()
            ));
        }
    }
    let mut opts = ListOptions::default();
    while let Some(arg) = args.next() {
        if let Some(path) = take_config_arg(&arg, &mut args)? {
            if opts.config_path.replace(path).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        let Some(a) = arg.to_str() else {
            return Err(format!(
                "無法辨識的參數（含有不是有效 Unicode 的字元）: {}",
                arg.to_string_lossy()
            ));
        };
        let value_of = |name: &str, a: &str, next: Option<OsString>| -> Result<String, String> {
            match a.strip_prefix(&format!("{name}=")) {
                Some(v) => Ok(v.to_string()),
                None => next
                    .and_then(|v| v.into_string().ok())
                    .ok_or_else(|| format!("{name} 需要指定值")),
            }
        };
        match a {
            "-h" | "--help" => return Ok(HostsCommand::Help),
            "--json" => opts.json = true,
            "--ascii" => opts.ascii = true,
            "--wide" => opts.wide = true,
            "--no-header" => opts.no_header = true,
            _ if a == "--offline-for" || a.starts_with("--offline-for=") => {
                let next = if a.contains('=') { None } else { args.next() };
                let v = value_of("--offline-for", a, next)?;
                let d = parse_duration(&v).ok_or_else(|| {
                    format!("--offline-for 的值 \"{v}\" 無效，例如 90m、12h、3d、2w")
                })?;
                opts.offline_for = Some(d);
            }
            _ => return Err(format!("無法辨識的參數: {a}")),
        }
    }
    Ok(HostsCommand::List(opts))
}

/// 主機的狀態（cli-style.md 的 token 名稱）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostStatus {
    /// 排序依嚴重度：離線 → 未知 → 線上
    Offline,
    Unknown,
    Online,
}

/// 清冊記錄為線上、但這麼久沒有更新時改顯示「未知」：離線門檻 + 清冊同步的延遲
/// （`last_seen` 最多 60 秒寫入一次，同步每 30 秒一次）
fn stale_after(offline_after_ms: Option<i64>) -> chrono::Duration {
    let offline = chrono::Duration::milliseconds(offline_after_ms.unwrap_or(0).max(0));
    offline
        + chrono::Duration::seconds(60)
        + chrono::Duration::from_std(INVENTORY_SYNC_INTERVAL * 2).unwrap_or_default()
}

pub fn status_of(row: &HostRow, now: DateTime<Utc>) -> HostStatus {
    if !row.online {
        HostStatus::Offline
    } else if now - row.last_seen > stale_after(row.offline_after_ms) {
        HostStatus::Unknown
    } else {
        HostStatus::Online
    }
}

/// `--offline-for` 篩選：不是線上，且最後訊息至少在 `d` 之前
pub fn matches_filter(row: &HostRow, now: DateTime<Utc>, offline_for: Option<Duration>) -> bool {
    match offline_for {
        None => true,
        Some(d) => {
            status_of(row, now) != HostStatus::Online
                && (now - row.last_seen)
                    .to_std()
                    .is_ok_and(|silent| silent >= d)
        }
    }
}

/// 「3 小時 12 分前」等相對時間
pub fn relative(now: DateTime<Utc>, t: DateTime<Utc>) -> String {
    let secs = (now - t).num_seconds();
    if secs < 0 {
        return "剛剛".to_string();
    }
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match secs {
        0..60 => format!("{secs} 秒前"),
        60..3600 => format!("{m} 分前"),
        3600..86_400 => format!("{h} 小時 {m} 分前"),
        _ => format!("{d} 天 {h} 小時前"),
    }
}

/// peer（`1.2.3.4:5000`、`[::1]:5000`）中的 IP
fn ip_of(peer: &str) -> String {
    match peer.parse::<std::net::SocketAddr>() {
        Ok(a) => a.ip().to_string(),
        Err(_) => peer.to_string(),
    }
}

/// 顯示寬度：East Asian Wide／Fullwidth 的字元為 2 欄，其他為 1 欄（cli-style.md §6.2）
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let c = c as u32;
            let wide = matches!(c,
                0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
                | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
                | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x20000..=0x3FFFD);
            if wide { 2 } else { 1 }
        })
        .sum()
}

/// 輸出前跳脫控制字元（避免終端機跳脫序列注入）
fn escape_text(s: &str) -> String {
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

/// 是否使用 Unicode 符號（cli-style.md §4.2）
pub fn use_unicode(ascii_flag: bool) -> bool {
    if ascii_flag {
        return false;
    }
    match std::env::var("KAIROS_SYMBOLS").ok().as_deref() {
        Some("ascii") => return false,
        Some("unicode") => return true,
        _ => {}
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    if cfg!(windows) {
        let env = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        env("WT_SESSION")
            || std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "vscode")
            || std::env::var("ConEmuANSI").is_ok_and(|v| v == "ON")
            || env("TERM")
    } else {
        ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .is_some_and(|v| {
                let v = v.to_ascii_lowercase();
                v.contains("utf-8") || v.contains("utf8")
            })
    }
}

fn status_label(status: HostStatus, unicode: bool) -> &'static str {
    match (status, unicode) {
        (HostStatus::Online, true) => "✓ 線上",
        (HostStatus::Offline, true) => "○ 離線",
        (HostStatus::Unknown, true) => "? 未知",
        (HostStatus::Online, false) => "[ OK ] 線上",
        (HostStatus::Offline, false) => "[DOWN] 離線",
        (HostStatus::Unknown, false) => "[ ?? ] 未知",
    }
}

/// 以 2 個空格分隔、依顯示寬度補齊的表格（最後一欄不補空白）
fn render_table(rows: &[Vec<String>]) -> String {
    let cols = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..cols)
        .map(|i| rows.iter().map(|r| display_width(&r[i])).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            line.push_str(cell);
            if i + 1 < cols {
                line.push_str(&" ".repeat(widths[i] - display_width(cell) + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 一台主機的狀態與資料
struct Listed<'a> {
    row: &'a HostRow,
    status: HostStatus,
}

/// 人看的輸出
pub fn render_human(
    rows: &[HostRow],
    now: DateTime<Utc>,
    opts: &ListOptions,
    unicode: bool,
) -> String {
    let mut listed: Vec<Listed> = rows
        .iter()
        .filter(|r| matches_filter(r, now, opts.offline_for))
        .map(|row| Listed {
            row,
            status: status_of(row, now),
        })
        .collect();
    // 依嚴重度，再依最後訊息由舊到新
    listed.sort_by(|a, b| {
        (a.status, a.row.last_seen, &a.row.agent_id).cmp(&(
            b.status,
            b.row.last_seen,
            &b.row.agent_id,
        ))
    });
    if listed.is_empty() {
        return if opts.no_header {
            String::new()
        } else {
            "沒有符合條件的主機。\n".to_string()
        };
    }
    let mut table = Vec::new();
    if !opts.no_header {
        let mut header = vec!["狀態", "主機名稱", "AGENT_ID", "IP", "最後訊息"];
        if opts.wide {
            header.extend(["首次出現", "版本", "協定", "身分"]);
        }
        table.push(header.into_iter().map(str::to_string).collect());
    }
    for l in &listed {
        let r = l.row;
        let agent_id = if opts.wide {
            r.agent_id.clone()
        } else {
            r.agent_id.chars().take(8).collect()
        };
        let name = match &r.display_name {
            Some(d) if opts.wide => format!("{}（{}）", escape_text(&r.hostname), escape_text(d)),
            _ => escape_text(&r.hostname),
        };
        let mut cells = vec![
            status_label(l.status, unicode).to_string(),
            name,
            agent_id,
            r.peer.as_deref().map(ip_of).unwrap_or_else(|| "-".into()),
            relative(now, r.last_seen),
        ];
        if opts.wide {
            cells.extend([
                fmt_time(r.first_seen),
                r.agent_version
                    .as_deref()
                    .map(escape_text)
                    .unwrap_or_else(|| "-".into()),
                r.proto.map_or_else(|| "-".into(), |p| format!("v{p}")),
                if r.identity_verified {
                    "mTLS"
                } else {
                    "agent_id"
                }
                .to_string(),
            ]);
        }
        table.push(cells);
    }
    let mut out = render_table(&table);
    if !opts.no_header {
        let count = |s| listed.iter().filter(|l| l.status == s).count();
        let mut parts = Vec::new();
        for (s, label) in [
            (HostStatus::Offline, "離線"),
            (HostStatus::Unknown, "未知"),
            (HostStatus::Online, "線上"),
        ] {
            let n = count(s);
            if n > 0 {
                parts.push(format!("{label} {n}"));
            }
        }
        out.push('\n');
        out.push_str(&format!("共 {} 台（{}）\n", listed.len(), parts.join("、")));
    }
    out
}

/// JSON 輸出中的一台主機
#[derive(Debug, Serialize)]
struct JsonHost<'a> {
    agent_id: &'a str,
    hostname: &'a str,
    display_name: Option<&'a str>,
    status: HostStatus,
    /// 最後訊息（Kairos 的時鐘；清冊同步最多延遲約 90 秒）
    last_seen: String,
    /// 執行本命令的主機時鐘減去 last_seen
    silent_secs: i64,
    first_seen: String,
    ip: Option<String>,
    peer: Option<&'a str>,
    agent_version: Option<&'a str>,
    proto: Option<i64>,
    host_fingerprint: Option<&'a str>,
    /// agent_id 是否以 mTLS 客戶端憑證驗證
    identity_verified: bool,
}

/// 非 ASCII 字元一律以 `\uXXXX` 跳脫（cli-style.md §7.1）。非 ASCII 只會出現在 JSON 字串內，
/// 序列化後再跳脫仍是合法的 JSON
pub fn ascii_json(value: &serde_json::Value) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// JSON 輸出
pub fn render_json(rows: &[HostRow], now: DateTime<Utc>, opts: &ListOptions) -> String {
    let mut hosts: Vec<(HostStatus, &HostRow)> = rows
        .iter()
        .filter(|r| matches_filter(r, now, opts.offline_for))
        .map(|r| (status_of(r, now), r))
        .collect();
    hosts.sort_by(|a, b| {
        (a.0, a.1.last_seen, &a.1.agent_id).cmp(&(b.0, b.1.last_seen, &b.1.agent_id))
    });
    let count = |s| hosts.iter().filter(|(st, _)| *st == s).count();
    let summary = serde_json::json!({
        "total": hosts.len(),
        "online": count(HostStatus::Online),
        "offline": count(HostStatus::Offline),
        "unknown": count(HostStatus::Unknown),
    });
    let list: Vec<JsonHost> = hosts
        .iter()
        .map(|(status, r)| JsonHost {
            agent_id: &r.agent_id,
            hostname: &r.hostname,
            display_name: r.display_name.as_deref(),
            status: *status,
            last_seen: fmt_time(r.last_seen),
            silent_secs: (now - r.last_seen).num_seconds().max(0),
            first_seen: fmt_time(r.first_seen),
            ip: r.peer.as_deref().map(ip_of),
            peer: r.peer.as_deref(),
            agent_version: r.agent_version.as_deref(),
            proto: r.proto,
            host_fingerprint: r.host_fingerprint.as_deref(),
            identity_verified: r.identity_verified,
        })
        .collect();
    let doc = serde_json::json!({
        "schema": JSON_SCHEMA,
        "ok": true,
        "generated_at": fmt_time(now),
        "data": {
            "filter": { "offline_for_secs": opts.offline_for.map(|d| d.as_secs()) },
            "hosts": list,
            "summary": summary,
        },
        "warnings": [],
        "error": null,
    });
    ascii_json(&doc) + "\n"
}

/// 命令本身無法完成時的錯誤（KRS-E4003「無法載入主機清單」，或設定錯誤）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub code: &'static str,
    pub title: String,
    pub cause: String,
    pub fix: Vec<String>,
    pub exit: u8,
}

impl CliError {
    pub fn render_human(&self, unicode: bool) -> String {
        let mark = if unicode { "✗" } else { "[FAIL]" };
        let mut out = format!(
            "{mark} 錯誤 {}：{}\n  原因：{}\n  處理方式：\n",
            self.code, self.title, self.cause
        );
        for (i, f) in self.fix.iter().enumerate() {
            out.push_str(&format!("    {}. {}\n", i + 1, f));
        }
        out
    }

    pub fn render_json(&self, now: DateTime<Utc>) -> String {
        let doc = serde_json::json!({
            "schema": JSON_SCHEMA,
            "ok": false,
            "generated_at": fmt_time(now),
            "data": null,
            "warnings": [],
            "error": {
                "code": self.code,
                "severity": "error",
                "title": self.title,
                "cause": self.cause,
                "fix": self.fix,
                "details": {},
                "explain": null,
            },
        });
        ascii_json(&doc) + "\n"
    }
}

/// 結束代碼（cli-style.md §8）
pub const EXIT_GENERAL: u8 = 1;
const EXIT_CONFIG: u8 = 3;
const EXIT_PERMISSION: u8 = 10;

/// 載入設定並以唯讀方式開啟資料庫（`kairos hosts`、`kairos reports` 共用）。失敗時以 `code`、
/// `title` 回報（例如 KRS-E4003「無法載入主機清單」）
pub fn open_db(
    config_path: Option<&Path>,
    code: &'static str,
    title: &str,
) -> Result<rusqlite::Connection, CliError> {
    let config = AppConfig::load(config_path).map_err(|e| CliError {
        code: "KRS-E0201",
        title: "設定檔無法載入".to_string(),
        cause: e.to_string(),
        fix: vec!["修正設定檔後再執行，或以 --config 指定 Kairos 使用的設定檔。".to_string()],
        exit: EXIT_CONFIG,
    })?;
    if config.storage.backend != StorageBackend::Sqlite {
        return Err(CliError {
            code,
            title: title.to_string(),
            cause: "storage.backend 為 \"jsonl\"，沒有 SQLite 資料庫。".to_string(),
            fix: vec![
                "主機清冊與 SQLite 中的 report 需要 storage.backend = \"sqlite\"（預設值）；改回預設並重新啟動 Kairos 後才會寫入。".to_string(),
            ],
            exit: EXIT_CONFIG,
        });
    }
    let path = Path::new(&config.storage.data_dir).join(DB_FILE);
    let failed = |e: std::io::Error| {
        let (fix, exit) = match e.kind() {
            ErrorKind::NotFound => (
                vec![
                    "確認 --config 指向 Kairos 使用的設定檔，或在 Kairos 的工作目錄執行（storage.data_dir 為相對路徑時以目前目錄為準）。".to_string(),
                    "資料庫在 Kairos 第一次以 storage.backend = \"sqlite\" 啟動時建立。".to_string(),
                ],
                EXIT_GENERAL,
            ),
            ErrorKind::PermissionDenied => (
                vec!["以 root 或 Kairos 的服務帳號執行，例如：sudo -u kairos kairos hosts list --config /opt/kairos/kairos.toml".to_string()],
                EXIT_PERMISSION,
            ),
            _ => (
                vec!["確認 storage.data_dir 指向 Kairos 的資料目錄；資料庫損毀時請從備份還原（kairos/STORAGE.md「備份與還原」）。".to_string()],
                EXIT_GENERAL,
            ),
        };
        CliError {
            code,
            title: title.to_string(),
            cause: e.to_string(),
            fix,
            exit,
        }
    };
    open_read_only(&path).map_err(failed)
}

/// 主機清單的查詢失敗
const LIST_ERROR: (&str, &str) = ("KRS-E4003", "無法載入主機清單");

/// 載入設定、開啟資料庫並讀出清冊
fn load_rows(config_path: Option<&Path>) -> Result<Vec<HostRow>, CliError> {
    let conn = open_db(config_path, LIST_ERROR.0, LIST_ERROR.1)?;
    list_hosts(&conn).map_err(|e| CliError {
        code: LIST_ERROR.0,
        title: LIST_ERROR.1.to_string(),
        cause: e.to_string(),
        fix: vec!["確認 storage.data_dir 指向 Kairos 的資料目錄；資料庫損毀時請從備份還原（kairos/STORAGE.md「備份與還原」）。".to_string()],
        exit: EXIT_GENERAL,
    })
}

/// 執行 `kairos hosts ...`，回傳結束代碼
pub fn run(cmd: HostsCommand) -> u8 {
    let opts = match cmd {
        HostsCommand::Help => {
            println!("{HOSTS_USAGE}");
            return 0;
        }
        HostsCommand::List(opts) => opts,
    };
    let now = Utc::now();
    let unicode = !opts.json && use_unicode(opts.ascii);
    let mut stdout = std::io::stdout().lock();
    match load_rows(opts.config_path.as_deref()) {
        Ok(rows) => {
            let text = if opts.json {
                render_json(&rows, now, &opts)
            } else {
                render_human(&rows, now, &opts, unicode)
            };
            match stdout
                .write_all(text.as_bytes())
                .and_then(|_| stdout.flush())
            {
                Ok(()) => 0,
                // 輸出被關閉（例如 | head）不視為錯誤
                Err(e) if e.kind() == ErrorKind::BrokenPipe => 0,
                Err(_) => EXIT_GENERAL,
            }
        }
        Err(e) => {
            if opts.json {
                let _ = stdout.write_all(e.render_json(now).as_bytes());
            } else {
                let unicode_err = !opts.ascii && std::io::stderr().is_terminal() && unicode;
                eprint!("{}", e.render_human(unicode_err));
            }
            e.exit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn os(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 12, 6, 0, 0).unwrap()
    }

    fn row(agent_id: &str, hostname: &str, online: bool, silent: chrono::Duration) -> HostRow {
        HostRow {
            agent_id: agent_id.into(),
            hostname: hostname.into(),
            display_name: None,
            agent_version: Some("0.2.0".into()),
            proto: Some(2),
            host_fingerprint: None,
            peer: Some("10.0.3.4:50000".into()),
            identity_verified: true,
            online,
            first_seen: now() - chrono::Duration::days(30),
            last_seen: now() - silent,
            offline_after_ms: Some(75_000),
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("3d"), Some(Duration::from_secs(3 * 86_400)));
        assert_eq!(parse_duration("90m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("1d12h"), Some(Duration::from_secs(129_600)));
        assert_eq!(parse_duration("2w"), Some(Duration::from_secs(14 * 86_400)));
        assert_eq!(parse_duration("45s"), Some(Duration::from_secs(45)));
        for bad in ["", "3", "d", "3x", "-1d", "3d4", "99999999999999999999d"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn argument_parsing() {
        assert_eq!(parse_args(os(&["--help"])), Ok(HostsCommand::Help));
        assert_eq!(parse_args(os(&["list", "-h"])), Ok(HostsCommand::Help));
        assert_eq!(
            parse_args(os(&["list"])),
            Ok(HostsCommand::List(ListOptions::default()))
        );
        assert_eq!(
            parse_args(os(&[
                "list",
                "--offline-for",
                "3d",
                "--json",
                "--config=k.toml",
                "--wide",
                "--no-header",
                "--ascii"
            ])),
            Ok(HostsCommand::List(ListOptions {
                config_path: Some(PathBuf::from("k.toml")),
                offline_for: Some(Duration::from_secs(3 * 86_400)),
                json: true,
                ascii: true,
                wide: true,
                no_header: true,
            }))
        );
        assert_eq!(
            parse_args(os(&["list", "--offline-for=12h"])),
            Ok(HostsCommand::List(ListOptions {
                offline_for: Some(Duration::from_secs(12 * 3600)),
                ..Default::default()
            }))
        );
        for bad in [
            &[][..],
            &["show"],
            &["list", "--offline-for"],
            &["list", "--offline-for", "soon"],
            &["list", "--bogus"],
            &["list", "--config"],
            &["list", "--config", "a", "--config", "b"],
        ] {
            assert!(parse_args(os(bad)).is_err(), "{bad:?}");
        }
        // 從頂層解析
        assert!(matches!(
            crate::config::parse_args(os(&["hosts", "list"])),
            Ok(crate::config::CliAction::Hosts(HostsCommand::List(_)))
        ));
    }

    #[test]
    fn status_and_offline_filter() {
        let n = now();
        let online = row("a", "web-01", true, chrono::Duration::seconds(30));
        let stale = row("b", "web-02", true, chrono::Duration::hours(1));
        let gone = row("c", "db-01", false, chrono::Duration::days(4));
        let recent = row("d", "db-02", false, chrono::Duration::hours(2));
        assert_eq!(status_of(&online, n), HostStatus::Online);
        assert_eq!(status_of(&stale, n), HostStatus::Unknown);
        assert_eq!(status_of(&gone, n), HostStatus::Offline);
        let three_days = Some(Duration::from_secs(3 * 86_400));
        let hit: Vec<&str> = [&online, &stale, &gone, &recent]
            .into_iter()
            .filter(|r| matches_filter(r, n, three_days))
            .map(|r| r.agent_id.as_str())
            .collect();
        assert_eq!(hit, ["c"]);
        // 失聯 30 分鐘以上：未知與離線，線上的不算
        let hit = [&online, &stale, &gone, &recent]
            .into_iter()
            .filter(|r| matches_filter(r, n, Some(Duration::from_secs(1800))))
            .count();
        assert_eq!(hit, 3);
    }

    #[test]
    fn relative_times() {
        let n = now();
        let ago = |s| relative(n, n - chrono::Duration::seconds(s));
        assert_eq!(ago(8), "8 秒前");
        assert_eq!(ago(125), "2 分前");
        assert_eq!(ago(3 * 3600 + 12 * 60), "3 小時 12 分前");
        assert_eq!(ago(4 * 86_400 + 3600), "4 天 1 小時前");
        assert_eq!(relative(n, n + chrono::Duration::seconds(5)), "剛剛");
    }

    #[test]
    fn human_table_is_aligned_sorted_and_summarized() {
        let n = now();
        let rows = vec![
            row(
                "6f1c0e4a-1b2c-4d3e-9f00-112233445566",
                "file-srv",
                true,
                chrono::Duration::seconds(2),
            ),
            row(
                "0a9b8c7d-0000-4000-8000-000000000001",
                "主機-資料庫",
                false,
                chrono::Duration::hours(3) + chrono::Duration::minutes(12),
            ),
            row(
                "91d2e0aa-0000-4000-8000-000000000002",
                "plc\u{1b}[2J",
                false,
                chrono::Duration::days(4),
            ),
        ];
        let out = render_human(&rows, n, &ListOptions::default(), false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["狀態", "主機名稱", "AGENT_ID", "IP", "最後訊息"]
        );
        // 離線依最後訊息由舊到新，最後是線上
        assert!(lines[1].starts_with("[DOWN] 離線"), "{out}");
        assert!(
            lines[1].contains("91d2e0aa") && lines[1].contains("4 天 0 小時前"),
            "{out}"
        );
        assert!(
            lines[2].contains("0a9b8c7d") && lines[2].contains("3 小時 12 分前"),
            "{out}"
        );
        assert!(
            lines[3].starts_with("[ OK ] 線上") && lines[3].contains("10.0.3.4"),
            "{out}"
        );
        // 控制字元已跳脫
        assert!(!out.contains('\u{1b}') && out.contains("\\u{1b}"), "{out}");
        // 依顯示寬度對齊：AGENT_ID 欄在每一行的顯示位置相同
        let col = |l: &str| {
            display_width(
                &l[..l
                    .find("AGENT_ID")
                    .or_else(|| l.find("91d2"))
                    .or_else(|| l.find("0a9b"))
                    .or_else(|| l.find("6f1c"))
                    .unwrap()],
            )
        };
        assert_eq!(col(lines[0]), col(lines[1]));
        assert_eq!(col(lines[1]), col(lines[2]));
        assert_eq!(col(lines[2]), col(lines[3]));
        assert_eq!(lines.last().unwrap(), &"共 3 台（離線 2、線上 1）");

        // Unicode 符號、篩選、空結果
        let out = render_human(
            &rows,
            n,
            &ListOptions {
                offline_for: Some(Duration::from_secs(86_400)),
                ..Default::default()
            },
            true,
        );
        assert!(out.starts_with("狀態"));
        assert!(out.contains("○ 離線") && !out.contains("線上"), "{out}");
        let out = render_human(
            &rows,
            n,
            &ListOptions {
                offline_for: Some(Duration::from_secs(30 * 86_400)),
                ..Default::default()
            },
            true,
        );
        assert_eq!(out, "沒有符合條件的主機。\n");
        let out = render_human(
            &rows,
            n,
            &ListOptions {
                no_header: true,
                wide: true,
                ..Default::default()
            },
            false,
        );
        assert_eq!(out.lines().count(), 3);
        assert!(
            out.contains("6f1c0e4a-1b2c-4d3e-9f00-112233445566")
                && out.contains("mTLS")
                && out.contains("v2"),
            "{out}"
        );
    }

    #[test]
    fn json_is_ascii_and_has_the_envelope() {
        let n = now();
        let rows = vec![
            row("a", "主機-01", false, chrono::Duration::days(5)),
            row("b", "web", true, chrono::Duration::seconds(1)),
        ];
        let opts = ListOptions {
            offline_for: Some(Duration::from_secs(3 * 86_400)),
            json: true,
            ..Default::default()
        };
        let text = render_json(&rows, n, &opts);
        assert!(text.is_ascii(), "{text}");
        assert!(text.ends_with('\n') && text.lines().count() == 1);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["schema"], JSON_SCHEMA);
        assert_eq!(v["ok"], true);
        assert_eq!(v["error"], serde_json::Value::Null);
        assert_eq!(v["warnings"], serde_json::json!([]));
        assert_eq!(v["generated_at"], "2026-10-12T06:00:00.000Z");
        assert_eq!(v["data"]["filter"]["offline_for_secs"], 259_200);
        let hosts = v["data"]["hosts"].as_array().unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0]["hostname"], "主機-01");
        assert_eq!(hosts[0]["status"], "offline");
        assert_eq!(hosts[0]["silent_secs"], 5 * 86_400);
        assert_eq!(hosts[0]["ip"], "10.0.3.4");
        assert_eq!(v["data"]["summary"]["total"], 1);

        let e = CliError {
            code: "KRS-E4003",
            title: "無法載入主機清單".into(),
            cause: "找不到".into(),
            fix: vec!["檢查".into()],
            exit: 1,
        };
        let text = e.render_json(n);
        assert!(text.is_ascii());
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["data"], serde_json::Value::Null);
        assert_eq!(v["error"]["code"], "KRS-E4003");
        assert_eq!(v["error"]["title"], "無法載入主機清單");
        let human = e.render_human(false);
        assert!(human.starts_with("[FAIL] 錯誤 KRS-E4003：無法載入主機清單\n  原因：找不到\n  處理方式：\n    1. 檢查\n"), "{human}");
        // U+1F600 以 surrogate pair 跳脫
        assert_eq!(ascii_json(&serde_json::json!("😀")), "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn widths() {
        assert_eq!(display_width("web-01"), 6);
        assert_eq!(display_width("主機"), 4);
        assert_eq!(display_width("ｗ"), 2);
        assert_eq!(ip_of("[::1]:5000"), "::1");
        assert_eq!(ip_of("10.0.0.1:1"), "10.0.0.1");
        assert_eq!(ip_of("weird"), "weird");
    }
}
