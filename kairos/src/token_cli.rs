//! `kairos api-token create|list|revoke`：主控台 API 的 token（`kairos/API.md` 第 4.2 節）。
//!
//! 輸出依設計包 `design/design-system/cli-style.md`：結果在 stdout（`create` 只有 token 一行，方便
//! `TOKEN=$(...)`），過程、說明與錯誤在 stderr；`--json` 為純 ASCII 的單一 JSON 文件
//! （`schema` 為 `kairos.api-token.<子命令>/v1`），錯誤也以 JSON 寫到 stdout。`revoke` 是 L1 的危險操作
//! （§5.4）：互動時詢問，非互動需要 `--yes`。

use chrono::{DateTime, FixedOffset, Local, NaiveDate, Offset, Utc};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::api::tokens::{self, NewToken, Revoked, TokenGrant, TokenRow, TokenState};
use crate::config::{AppConfig, StorageBackend, take_config_arg};
use crate::hosts_cli::{ascii_json, display_width, parse_duration, use_unicode};
use crate::rbac::{LegacyScope, Role};
use crate::storage::fmt_time;
use crate::store::sqlite::{DB_FILE, SqliteOptions, open_admin};

/// `kairos api-token --help`
pub const TOKEN_USAGE: &str = "kairos api-token — 主控台 API（kairos/API.md）的 token

用法：
  kairos api-token create --name <名稱> [--role <角色> | --user <帳號> [--role <角色>]] [--expires-in <時間> | --expires-at <日期>] [選項]
  kairos api-token list [--all] [選項]
  kairos api-token revoke <token_id> [--reason <原因>] [--yes] [選項]

create 的選項：
  --name <名稱>          用途說明（1～64 個字元），例如 grafana、console-dev
  --expires-in <時間>    多久後到期，例如 90d、12h、2w（預設 90d，上限 3650d）
  --expires-at <日期>    到期日：2027-01-01（UTC 當天 00:00）或 RFC 3339 時間
  --role <角色>          token 的角色：viewer（預設，唯讀）、auditor、operator、admin（全站；kairos/API.md 第 12.1 節）
  --user <帳號>          token 屬於這個使用者：權限＝使用者目前的權限（另有 --role 時取交集）；
                         使用者停用或刪除時 token 立即失效
  --scope <範圍>         〔已棄用〕0.5 的範圍 read、operate、admin（權限凍結在 0.5；改用 --role）

list 的選項：
  --all                  也列出已撤銷與已過期的 token

revoke 的選項：
  --reason <原因>        撤銷原因（寫入資料庫與 Kairos 的日誌）
  --yes                  不詢問直接撤銷（非互動執行時必須）

通用選項：
  --config <path>        Kairos 的設定檔（用來找到 storage.data_dir；預設為目前目錄的 kairos.toml）
  --json                 輸出機器可讀的 JSON（純 ASCII）
  --ascii                不使用 Unicode 符號
  -h, --help             顯示本說明

token 只在建立時顯示一次（寫到 stdout），資料庫只保存它的 SHA-256；不提供不過期的 token，
需要長期使用時以 --expires-in 3650d 建立，list 出現 14 天內到期的提示時輪替。時間以本機時區顯示
（--json 為 UTC）。請以 Kairos 的服務帳號執行，
Kairos 執行中也可以；建立與撤銷由執行中的 Kairos 在 10 秒內寫入日誌。

範例：
  sudo -u kairos /opt/kairos/kairos api-token create --name grafana --expires-in 180d --config /opt/kairos/kairos.toml
  sudo -u kairos /opt/kairos/kairos api-token list --config /opt/kairos/kairos.toml
  sudo -u kairos /opt/kairos/kairos api-token revoke 3f9c2a71b4e05d18 --reason 離職 --yes --config /opt/kairos/kairos.toml

結束代碼：0 完成、1 資料庫錯誤或找不到 token、2 用法錯誤、3 設定錯誤、10 權限不足、11 資料庫使用中";

/// 預設的有效期限（天）
pub const DEFAULT_EXPIRY_DAYS: u64 = 90;
/// 有效期限的上限（天）
pub const MAX_EXPIRY_DAYS: u64 = 3650;
/// `list` 對剩餘天數在這以內的有效 token 加 `⚠`（cli-style.md §11.6）
pub const EXPIRY_WARN_DAYS: i64 = 14;

const EXIT_GENERAL: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_CONFIG: u8 = 3;
const EXIT_PERMISSION: u8 = 10;
const EXIT_CONFLICT: u8 = 11;

/// 到期的指定方式
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expiry {
    /// 未指定：預設 90 天
    Default,
    In(std::time::Duration),
    At(DateTime<Utc>),
}

/// token 的權限來源（命令列；`--user` 在執行時查詢使用者）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantSpec {
    /// `--scope`（已棄用）
    Legacy(LegacyScope),
    /// `--role`
    Role(Role),
    /// 沒有 `--role`、`--user`：唯讀（viewer），並在 stderr 提示（`cli-style.md` §11.6）
    DefaultViewer,
    /// `--user`（＋選用的 `--role`）
    User {
        username: String,
        role: Option<Role>,
    },
}

/// 子命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenAction {
    Help,
    Create {
        name: String,
        expiry: Expiry,
        grant: GrantSpec,
    },
    List {
        all: bool,
    },
    Revoke {
        token_id: String,
        reason: Option<String>,
        yes: bool,
    },
}

/// `kairos api-token` 的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenCommand {
    pub action: TokenAction,
    pub config_path: Option<PathBuf>,
    pub json: bool,
    pub ascii: bool,
}

fn parse_at(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc());
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// 解析 `kairos api-token` 之後的參數
pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<TokenCommand, String> {
    let mut args = args.peekable();
    let sub = match args.next() {
        None => return Err("kairos api-token 需要子命令（create、list、revoke）".to_string()),
        Some(a) if a == "-h" || a == "--help" => {
            return Ok(TokenCommand {
                action: TokenAction::Help,
                config_path: None,
                json: false,
                ascii: false,
            });
        }
        Some(a) => a.to_string_lossy().into_owned(),
    };
    if !matches!(sub.as_str(), "create" | "list" | "revoke") {
        return Err(format!("無法辨識的子命令: {sub}（create、list、revoke）"));
    }
    let mut config_path = None;
    let (mut json, mut ascii, mut all, mut yes) = (false, false, false, false);
    let mut name = None;
    let mut scope: Option<LegacyScope> = None;
    let mut role: Option<Role> = None;
    let mut user: Option<String> = None;
    let mut reason = None;
    let mut expires_in = None;
    let mut expires_at = None;
    let mut positional = Vec::new();
    while let Some(arg) = args.next() {
        if let Some(path) = take_config_arg(&arg, &mut args)? {
            if config_path.replace(path).is_some() {
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
        let a = a.to_string();
        let mut value = |flag: &str| -> Result<String, String> {
            match a.strip_prefix(&format!("{flag}=")) {
                Some(v) => Ok(v.to_string()),
                None => args
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .ok_or_else(|| format!("{flag} 需要指定值")),
            }
        };
        let flag = a.split('=').next().unwrap_or("").to_string();
        match flag.as_str() {
            "-h" | "--help" => {
                return Ok(TokenCommand {
                    action: TokenAction::Help,
                    config_path,
                    json,
                    ascii,
                });
            }
            "--json" => json = true,
            "--ascii" => ascii = true,
            "--all" if sub == "list" => all = true,
            "--yes" if sub == "revoke" => yes = true,
            // 設計包不提供不過期的 token（cli-style.md §11.6、decisions/INTEGRATION.md §26）
            "--no-expiry" if sub == "create" => {
                return Err(format!(
                    "不提供不過期的 API token（--no-expiry 已移除）；需要長期使用時以 --expires-in {MAX_EXPIRY_DAYS}d 建立，並在到期前輪替"
                ));
            }
            "--name" if sub == "create" => name = Some(value("--name")?),
            "--reason" if sub == "revoke" => reason = Some(value("--reason")?),
            "--scope" if sub == "create" => {
                let s = value("--scope")?;
                scope = Some(LegacyScope::parse(&s).ok_or_else(|| {
                    format!(
                        "--scope 的值 \"{s}\" 無效（read、operate、admin；已棄用，建議改用 --role）"
                    )
                })?);
            }
            "--role" if sub == "create" => {
                let r = value("--role")?;
                role = Some(Role::parse(&r).ok_or_else(|| {
                    format!("--role 的值 \"{r}\" 無效（viewer、auditor、operator、admin）")
                })?);
            }
            "--user" if sub == "create" => user = Some(value("--user")?),
            "--expires-in" if sub == "create" => {
                let v = value("--expires-in")?;
                let d = parse_duration(&v)
                    .filter(|d| !d.is_zero() && d.as_secs() <= MAX_EXPIRY_DAYS * 86_400)
                    .ok_or_else(|| {
                        format!("--expires-in 的值 \"{v}\" 無效，例如 90d、12h、2w（上限 {MAX_EXPIRY_DAYS}d）")
                    })?;
                expires_in = Some(d);
            }
            "--expires-at" if sub == "create" => {
                let v = value("--expires-at")?;
                expires_at = Some(parse_at(&v).ok_or_else(|| {
                    format!(
                        "--expires-at 的值 \"{v}\" 無效，例如 2027-01-01 或 2027-01-01T00:00:00Z"
                    )
                })?);
            }
            _ if a.starts_with('-') => return Err(format!("無法辨識的參數: {a}")),
            _ => positional.push(a.clone()),
        }
    }
    let action = match sub.as_str() {
        "create" => {
            if !positional.is_empty() {
                return Err(format!("無法辨識的參數: {}", positional[0]));
            }
            let name = name.ok_or("kairos api-token create 需要 --name <名稱>")?;
            tokens::validate_name(&name).map_err(|e| format!("--name 無效：{e}"))?;
            let expiry = match (expires_in, expires_at) {
                (None, None) => Expiry::Default,
                (Some(d), None) => Expiry::In(d),
                (None, Some(t)) => Expiry::At(t),
                (Some(_), Some(_)) => {
                    return Err("--expires-in、--expires-at 只能擇一".to_string());
                }
            };
            let grant = match (scope, role, user) {
                (Some(_), _, Some(_)) | (Some(_), Some(_), _) => {
                    return Err("--scope（已棄用）不能與 --role、--user 一起使用".to_string());
                }
                (Some(s), None, None) => GrantSpec::Legacy(s),
                (None, role, Some(username)) => GrantSpec::User { username, role },
                (None, Some(role), None) => GrantSpec::Role(role),
                (None, None, None) => GrantSpec::DefaultViewer,
            };
            TokenAction::Create {
                name,
                expiry,
                grant,
            }
        }
        "list" => {
            if !positional.is_empty() {
                return Err(format!("無法辨識的參數: {}", positional[0]));
            }
            TokenAction::List { all }
        }
        _ => {
            let [token_id] = positional.as_slice() else {
                return Err(
                    "kairos api-token revoke 需要一個 token_id（kairos api-token list 可查）"
                        .to_string(),
                );
            };
            let token_id = token_id.trim().to_ascii_lowercase();
            if token_id.len() != tokens::TOKEN_ID_LEN
                || !token_id.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(format!(
                    "token_id \"{token_id}\" 無效（16 個十六進位字元，不是完整的 token）"
                ));
            }
            TokenAction::Revoke {
                token_id,
                reason,
                yes,
            }
        }
    };
    Ok(TokenCommand {
        action,
        config_path,
        json,
        ascii,
    })
}

/// 命令失敗
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) code: Option<&'static str>,
    pub(crate) title: String,
    pub(crate) cause: String,
    pub(crate) fix: Vec<String>,
    pub(crate) exit: u8,
}

impl Failure {
    pub(crate) fn new(
        title: impl Into<String>,
        cause: impl Into<String>,
        fix: Vec<String>,
        exit: u8,
    ) -> Self {
        Self {
            code: None,
            title: title.into(),
            cause: cause.into(),
            fix,
            exit,
        }
    }

    pub(crate) fn human(&self, unicode: bool) -> String {
        let mark = if unicode { "✗" } else { "[FAIL]" };
        let code = self.code.map(|c| format!(" {c}")).unwrap_or_default();
        let mut out = format!(
            "{mark} 錯誤{code}：{}\n  原因：{}\n",
            self.title, self.cause
        );
        if !self.fix.is_empty() {
            out.push_str("  處理方式：\n");
            for (i, f) in self.fix.iter().enumerate() {
                out.push_str(&format!("    {}. {}\n", i + 1, f));
            }
        }
        out
    }

    pub(crate) fn json(&self, schema: &str, now: DateTime<Utc>) -> String {
        ascii_json(&json!({
            "schema": schema,
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
        })) + "\n"
    }
}

/// 執行命令的作業系統帳號（稽核用）
pub(crate) fn operator() -> String {
    let user = ["SUDO_USER", "USER", "LOGNAME", "USERNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "（未知）".to_string());
    match crate::fsperm::effective_uid() {
        Some(uid) => format!("{user}（uid {uid}）"),
        None => user,
    }
}

/// 以 root 執行而資料目錄屬於其他帳號時拒絕（新建立的資料庫檔會屬於 root，Kairos 之後無法寫入）
fn refuse_root(data_dir: &Path) -> Option<Failure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if crate::fsperm::effective_uid() != Some(0) {
            return None;
        }
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
            Failure::new(
                "無法寫入資料庫：請以 Kairos 的服務帳號執行",
                format!(
                    "以 root 執行，但資料目錄 {} 屬於 uid {owner}：建立的資料庫檔會屬於 root，Kairos（服務帳號）之後無法寫入",
                    data_dir.display()
                ),
                vec!["以資料目錄的擁有者執行，例如：sudo -u kairos /opt/kairos/kairos api-token list --config /opt/kairos/kairos.toml".to_string()],
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

/// 載入設定並開啟資料庫（`create` 時必要時建立）；資料庫不存在且 `create` 為 `false` 時回 `Ok(None)`
pub(crate) fn open(
    config_path: Option<&Path>,
    create: bool,
) -> Result<Option<rusqlite::Connection>, Failure> {
    let config = AppConfig::load(config_path).map_err(|e| Failure {
        code: Some(crate::diag::config_load_code(&e, config_path)),
        ..Failure::new(
            "設定檔無法載入",
            e.to_string(),
            vec!["修正設定檔後再執行，或以 --config 指定 Kairos 使用的設定檔。".to_string()],
            EXIT_CONFIG,
        )
    })?;
    if config.storage.backend != StorageBackend::Sqlite {
        return Err(Failure::new(
            "這個命令需要 SQLite",
            "storage.backend 為 \"jsonl\"：主控台 API、token、註冊與 PKI 的資料存在 SQLite 資料庫中。",
            vec!["改回 storage.backend = \"sqlite\"（預設值）並重新啟動 Kairos。".to_string()],
            EXIT_CONFIG,
        ));
    }
    let data_dir = PathBuf::from(&config.storage.data_dir);
    if !create && !data_dir.join(DB_FILE).is_file() {
        return Ok(None);
    }
    if let Some(f) = refuse_root(&data_dir) {
        return Err(f);
    }
    let opts = SqliteOptions::from_config(&config, &data_dir);
    open_admin(&opts, std::time::Duration::from_secs(5))
        .map(Some)
        .map_err(|e| {
            let (fix, exit) = match e.kind() {
                ErrorKind::PermissionDenied => (
                    vec!["以 Kairos 的服務帳號執行，例如：sudo -u kairos /opt/kairos/kairos api-token list --config /opt/kairos/kairos.toml".to_string()],
                    EXIT_PERMISSION,
                ),
                ErrorKind::ResourceBusy => (
                    vec!["kairos reports import 正在獨占資料庫；等它完成後再執行。".to_string()],
                    EXIT_CONFLICT,
                ),
                _ => (
                    vec!["確認 --config 指向 Kairos 使用的設定檔，以及 storage.data_dir 指向 Kairos 的資料目錄。".to_string()],
                    EXIT_GENERAL,
                ),
            };
            Failure::new("無法開啟資料庫", e.to_string(), fix, exit)
        })
}

pub(crate) fn db_failure(e: impl std::fmt::Display) -> Failure {
    Failure::new(
        "無法讀寫資料庫",
        e.to_string(),
        vec!["確認資料庫沒有損毀；必要時從備份還原（STORAGE.md「備份與還原」）。".to_string()],
        EXIT_GENERAL,
    )
}

/// 時區標示：`UTC+8`、`UTC-3:30`、`UTC`（copywriting.md §5.1）
pub(crate) fn zone_label(tz: FixedOffset) -> String {
    let secs = tz.local_minus_utc();
    if secs == 0 {
        return "UTC".to_string();
    }
    let sign = if secs < 0 { '-' } else { '+' };
    let (h, m) = (secs.abs() / 3600, secs.abs() % 3600 / 60);
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h}:{m:02}")
    }
}

/// 人看的時間（本機時區，不含時區標示；表格只在欄位標題標示）
pub(crate) fn human_time(ms: i64, tz: FixedOffset) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|t| t.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 人看的時間＋時區標示：`2027-04-11 14:03 UTC+8`
pub(crate) fn human_time_zoned(ms: i64, tz: FixedOffset) -> String {
    format!("{} {}", human_time(ms, tz), zone_label(tz))
}

/// 相對時間：`12 秒前`、`3 分鐘前`、`2 小時 15 分前`、`14 天前`（cli-style.md §11.6 的範例）
pub(crate) fn relative(now_ms: i64, ms: i64) -> String {
    let secs = (now_ms - ms).max(0) / 1000;
    match secs {
        0..60 => format!("{secs} 秒前"),
        60..3600 => format!("{} 分鐘前", secs / 60),
        3600..86_400 => format!("{} 小時 {} 分前", secs / 3600, secs % 3600 / 60),
        _ => format!("{} 天前", secs / 86_400),
    }
}

/// 「最後使用」欄：7 天內用相對時間，7 天以上改用本機時區的日期時間（copywriting.md §5.3）
fn last_used_text(now_ms: i64, ms: i64, tz: FixedOffset) -> String {
    if now_ms - ms >= 7 * 86_400_000 {
        human_time(ms, tz)
    } else {
        relative(now_ms, ms)
    }
}

/// 有效且剩餘 [`EXPIRY_WARN_DAYS`] 天以內
fn expires_soon(r: &TokenRow, now_ms: i64) -> bool {
    r.state(now_ms) == TokenState::Active
        && r.expires_at
            .is_some_and(|e| e - now_ms <= EXPIRY_WARN_DAYS * 86_400_000)
}

fn row_json(r: &TokenRow, now_ms: i64) -> Value {
    let t = |ms: Option<i64>| ms.and_then(DateTime::from_timestamp_millis).map(fmt_time);
    json!({
        "token_id": r.token_id,
        "name": r.name,
        "scope": r.scope,
        "role": r.role,
        "owner": r.owner_username,
        "grant": r.grant_label(),
        "state": r.state(now_ms).as_str(),
        "created_at": t(Some(r.created_at)),
        "created_by": r.created_by,
        "expires_at": t(r.expires_at),
        "revoked_at": t(r.revoked_at),
        "revoked_by": r.revoked_by,
        "revoke_reason": r.revoke_reason,
        "last_used_at": t(r.last_used_at),
        "last_used_ip": r.last_used_ip,
    })
}

fn state_label(s: TokenState) -> &'static str {
    match s {
        TokenState::Active => "有效",
        TokenState::Revoked => "已撤銷",
        TokenState::Expired => "已過期",
    }
}

/// 表格（2 個空格分隔、依顯示寬度對齊）
pub(crate) fn table(rows: &[Vec<String>]) -> String {
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

/// 輸出（stdout 與 stderr 的內容）與結束代碼
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit: u8,
}

/// 執行命令（不直接寫 stdout／stderr，方便測試）。人看的時間以 `tz` 顯示（`run` 傳入本機時區），
/// `--json` 一律 UTC。`confirm` 在互動式 `revoke` 時詢問使用者
pub fn execute(
    cmd: &TokenCommand,
    now: DateTime<Utc>,
    tz: FixedOffset,
    unicode: bool,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> Output {
    let schema = match &cmd.action {
        TokenAction::Create { .. } => "kairos.api-token.create/v1",
        TokenAction::List { .. } => "kairos.api-token.list/v1",
        TokenAction::Revoke { .. } | TokenAction::Help => "kairos.api-token.revoke/v1",
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
    let now_ms = now.timestamp_millis();
    let ok_mark = if unicode { "✓" } else { "[ OK ]" };
    match &cmd.action {
        TokenAction::Help => Output {
            stdout: format!("{TOKEN_USAGE}\n"),
            ..Default::default()
        },
        TokenAction::Create {
            name,
            expiry,
            grant,
        } => {
            let expires_at = match expiry {
                Expiry::Default => Some(now_ms + (DEFAULT_EXPIRY_DAYS * 86_400_000) as i64),
                Expiry::In(d) => Some(now_ms + i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
                Expiry::At(t) => {
                    if *t <= now {
                        return fail(Failure::new(
                            "到期時間已經過去",
                            format!(
                                "--expires-at {} 早於現在（{}）",
                                human_time_zoned(t.timestamp_millis(), tz),
                                human_time_zoned(now_ms, tz)
                            ),
                            vec!["指定未來的日期，或改用 --expires-in。".to_string()],
                            EXIT_USAGE,
                        ));
                    }
                    Some(t.timestamp_millis())
                }
            };
            let conn = match open(cmd.config_path.as_deref(), true) {
                Ok(Some(c)) => c,
                Ok(None) => return fail(db_failure("資料庫不存在")),
                Err(f) => return fail(f),
            };
            let by = operator();
            let grant = match grant {
                GrantSpec::Legacy(s) => TokenGrant::Legacy(*s),
                GrantSpec::Role(r) => TokenGrant::Role(*r),
                GrantSpec::DefaultViewer => TokenGrant::Role(Role::Viewer),
                GrantSpec::User { username, role } => {
                    match crate::users::store::find_user(&conn, username) {
                        Ok(Some(u)) if u.active() => TokenGrant::User {
                            user_id: u.user_id,
                            username: u.username,
                            role: *role,
                        },
                        Ok(Some(_)) => {
                            return fail(Failure::new(
                                format!("使用者 {username} 已停用"),
                                "停用的使用者不能擁有新的 API token",
                                vec!["先以 kairos users enable 啟用，或改用 --role。".to_string()],
                                EXIT_USAGE,
                            ));
                        }
                        Ok(None) => {
                            return fail(Failure::new(
                                format!("找不到使用者 {username}"),
                                "--user 必須是既有的帳號",
                                vec!["以 kairos users list 確認帳號。".to_string()],
                                EXIT_USAGE,
                            ));
                        }
                        Err(e) => return fail(db_failure(e)),
                    }
                }
            };
            let NewToken { token, row } =
                match tokens::create_with(&conn, name, &grant, expires_at, Some(&by), None, now_ms)
                {
                    Ok(t) => t,
                    Err(e) => return fail(db_failure(e)),
                };
            if cmd.json {
                let mut data = row_json(&row, now_ms);
                data["token"] = json!(token);
                // warnings 只放命令用法的警告（cli-style.md §7.2）：已棄用的 --scope
                let warnings: Vec<Value> = if matches!(grant, TokenGrant::Legacy(_)) {
                    vec![json!({
                        "code": "KRS-W0104", "severity": "warning",
                        "title": "--scope 已棄用",
                        "cause": "--scope 的權限凍結在 0.5 的範圍，不會得到之後新增的權限。",
                        "fix": ["改用 --role 或 --user 建立新的 token。"],
                        "details": { "flag": "--scope" }, "explain": null,
                    })]
                } else {
                    Vec::new()
                };
                return Output {
                    stdout: ascii_json(&json!({
                        "schema": schema, "ok": true, "generated_at": fmt_time(now),
                        "data": data, "warnings": warnings, "error": null,
                    })) + "\n",
                    ..Default::default()
                };
            }
            let expiry_text = row.expires_at.map_or_else(
                || "不過期".to_string(),
                |e| format!("到期 {}", human_time_zoned(e, tz)),
            );
            // 提示與警告（cli-style.md §5.2、§11.6）：未指定角色時說明已使用 viewer；--scope 以 KRS-W0104 提醒
            let mut notes = String::new();
            if matches!(
                cmd.action,
                TokenAction::Create {
                    grant: GrantSpec::DefaultViewer,
                    ..
                }
            ) {
                notes.push_str(&format!(
                    "{} 未指定 --role，使用唯讀（viewer）；需要讀取註冊、憑證與設定時請改用 --role auditor\n",
                    if unicode { "i" } else { "[INFO]" }
                ));
            }
            if matches!(grant, TokenGrant::Legacy(_)) {
                notes.push_str(&format!(
                    "{} 警告 KRS-W0104：--scope 已棄用（權限凍結在 0.5 的範圍）\n  處理方式：改用 --role 或 --user 建立新的 token。\n",
                    if unicode { "⚠" } else { "[WARN]" }
                ));
            }
            Output {
                stdout: format!("{token}\n"),
                stderr: format!(
                    "{ok_mark} 已建立 API token「{}」（token_id {}，權限 {}，{expiry_text}）\n  這是唯一一次顯示 token，請立即保存到密碼管理工具；遺失時撤銷後重新建立。\n  使用方式：Authorization: Bearer <token>（https://<主機>:9443/api/v1/）\n{notes}",
                    row.name,
                    row.token_id,
                    row.grant_label(),
                ),
                exit: 0,
            }
        }
        TokenAction::List { all } => {
            let rows = match open(cmd.config_path.as_deref(), false) {
                Ok(Some(conn)) => match tokens::list(&conn) {
                    Ok(rows) => rows,
                    Err(e) => return fail(db_failure(e)),
                },
                Ok(None) => Vec::new(),
                Err(f) => return fail(f),
            };
            let shown: Vec<&TokenRow> = rows
                .iter()
                .filter(|r| *all || r.state(now_ms) == TokenState::Active)
                .collect();
            if cmd.json {
                return Output {
                    stdout: ascii_json(&json!({
                        "schema": schema, "ok": true, "generated_at": fmt_time(now),
                        "data": { "tokens": shown.iter().map(|r| row_json(r, now_ms)).collect::<Vec<_>>() },
                        "warnings": [], "error": null,
                    })) + "\n",
                    ..Default::default()
                };
            }
            if shown.is_empty() {
                return Output {
                    stdout: if *all || rows.is_empty() {
                        "沒有 API token。\n".to_string()
                    } else {
                        "沒有有效的 API token（--all 另列出已撤銷與已過期的）。\n".to_string()
                    },
                    ..Default::default()
                };
            }
            // cli-style.md §11.6：時間為本機時區，只在欄位標題標示；14 天內到期的有效 token 加 ⚠
            let zone = zone_label(tz);
            let warn_mark = if unicode { "⚠" } else { "[WARN]" };
            let mut t = vec![vec![
                "TOKEN_ID".to_string(),
                "名稱".to_string(),
                "權限".to_string(),
                "狀態".to_string(),
                format!("建立（{zone}）"),
                format!("到期（{zone}）"),
                "最後使用".to_string(),
            ]];
            for r in &shown {
                t.push(vec![
                    r.token_id.clone(),
                    r.name
                        .chars()
                        .map(|c| if c.is_control() { '?' } else { c })
                        .collect(),
                    r.grant_label(),
                    state_label(r.state(now_ms)).to_string(),
                    format!(
                        "{} {}",
                        human_time(r.created_at, tz),
                        r.created_by.as_deref().unwrap_or("")
                    )
                    .trim_end()
                    .to_string(),
                    match r.expires_at {
                        // 0.4 草稿建立的不過期 token
                        None => "不過期".to_string(),
                        Some(e) if expires_soon(r, now_ms) => {
                            format!("{} {warn_mark}", human_time(e, tz))
                        }
                        Some(e) => human_time(e, tz),
                    },
                    match (r.last_used_at, &r.last_used_ip) {
                        (Some(t), Some(ip)) => {
                            format!("{}（{ip}）", last_used_text(now_ms, t, tz))
                        }
                        (Some(t), None) => last_used_text(now_ms, t, tz),
                        _ => "-".to_string(),
                    },
                ]);
            }
            let count = |s| shown.iter().filter(|r| r.state(now_ms) == s).count();
            let mut parts = vec![format!("有效 {}", count(TokenState::Active))];
            if *all {
                parts.push(format!("已撤銷 {}", count(TokenState::Revoked)));
                parts.push(format!("已過期 {}", count(TokenState::Expired)));
            }
            let soon = shown.iter().filter(|r| expires_soon(r, now_ms)).count();
            Output {
                stdout: format!(
                    "{}\n共 {} 個（{}）\n",
                    table(&t),
                    shown.len(),
                    parts.join("、")
                ),
                stderr: if soon > 0 {
                    format!(
                        "{warn_mark} {soon} 個 API token 在 {EXPIRY_WARN_DAYS} 天內到期：到期前以 kairos api-token create 建立新的 token、更新整合端，再撤銷舊的。\n"
                    )
                } else {
                    String::new()
                },
                exit: 0,
            }
        }
        TokenAction::Revoke {
            token_id,
            reason,
            yes,
        } => {
            let conn = match open(cmd.config_path.as_deref(), false) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    return fail(Failure::new(
                        format!("找不到 API token {token_id}"),
                        "資料庫還不存在（沒有任何 token）",
                        vec!["以 kairos api-token list 確認 token_id。".to_string()],
                        EXIT_GENERAL,
                    ));
                }
                Err(f) => return fail(f),
            };
            let row = match tokens::get(&conn, token_id) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return fail(Failure::new(
                        format!("找不到 API token {token_id}"),
                        "資料庫中沒有這個 token_id",
                        vec![
                            "以 kairos api-token list --all 確認 token_id（16 個十六進位字元）。"
                                .to_string(),
                        ],
                        EXIT_GENERAL,
                    ));
                }
                Err(e) => return fail(db_failure(e)),
            };
            if row.revoked_at.is_none() && !*yes {
                let prompt = format!(
                    "即將撤銷 API token「{}」（token_id {}）\n影響：\n  • 使用這個 token 的整合會立即無法存取主控台 API。\n  • 這個操作無法復原，並會寫入 Kairos 的日誌。\n是否繼續？[y/N] ",
                    row.name, row.token_id
                );
                if !confirm(&prompt) {
                    return fail(Failure::new(
                        "未撤銷",
                        "沒有確認（非互動執行時需要 --yes）",
                        vec![format!(
                            "確認要撤銷時加上 --yes：kairos api-token revoke {token_id} --yes"
                        )],
                        EXIT_USAGE,
                    ));
                }
            }
            let by = operator();
            let result = match tokens::revoke(&conn, token_id, Some(&by), reason.as_deref(), now_ms)
            {
                Ok(r) => r,
                Err(e) => return fail(db_failure(e)),
            };
            let (row, already) = match result {
                Revoked::Now(r) => (r, false),
                Revoked::Already(r) => (r, true),
                Revoked::NotFound => return fail(db_failure("token 已不存在")),
            };
            if cmd.json {
                let mut data = row_json(&row, now_ms);
                data["already_revoked"] = json!(already);
                return Output {
                    stdout: ascii_json(&json!({
                        "schema": schema, "ok": true, "generated_at": fmt_time(now),
                        "data": data, "warnings": [], "error": null,
                    })) + "\n",
                    ..Default::default()
                };
            }
            let text = if already {
                format!(
                    "{ok_mark} API token「{}」（token_id {}）先前已於 {} 撤銷\n",
                    row.name,
                    row.token_id,
                    row.revoked_at
                        .map(|t| human_time_zoned(t, tz))
                        .unwrap_or_default()
                )
            } else {
                format!(
                    "{ok_mark} 已撤銷 API token「{}」（token_id {}）。使用它的請求會立即收到 401（KRS-E6010）。\n",
                    row.name, row.token_id
                )
            };
            Output {
                stdout: text,
                ..Default::default()
            }
        }
    }
}

/// 執行 `kairos api-token ...`，回傳結束代碼
pub fn run(cmd: TokenCommand) -> u8 {
    let unicode = !cmd.json && use_unicode(cmd.ascii);
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut confirm = |prompt: &str| -> bool {
        if !interactive {
            return false;
        }
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes" | "YES")
    };
    let out = execute(
        &cmd,
        Utc::now(),
        Local::now().offset().fix(),
        unicode,
        &mut confirm,
    );
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

    /// 測試用的固定時區 UTC+8
    fn tz8() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).unwrap()
    }

    #[test]
    fn argument_parsing() {
        let c = parse_args(os(&[
            "create",
            "--name",
            "grafana",
            "--expires-in=30d",
            "--json",
        ]))
        .unwrap();
        assert_eq!(
            c.action,
            TokenAction::Create {
                name: "grafana".into(),
                expiry: Expiry::In(std::time::Duration::from_secs(30 * 86_400)),
                grant: GrantSpec::DefaultViewer,
            }
        );
        assert!(c.json);
        let c = parse_args(os(&["create", "--name=x", "--config", "k.toml"])).unwrap();
        assert_eq!(c.config_path, Some(PathBuf::from("k.toml")));
        assert!(matches!(
            c.action,
            TokenAction::Create {
                expiry: Expiry::Default,
                ..
            }
        ));
        // 不提供不過期的 token：以用法錯誤拒絕並提示 --expires-in 3650d
        let e = parse_args(os(&["create", "--name", "x", "--no-expiry"])).unwrap_err();
        assert!(e.contains("--expires-in 3650d"), "{e}");
        assert!(!TOKEN_USAGE.contains("--no-expiry"));
        let c = parse_args(os(&["create", "--name", "x", "--expires-at", "2027-01-01"])).unwrap();
        assert!(matches!(
            c.action,
            TokenAction::Create { expiry: Expiry::At(t), .. } if fmt_time(t) == "2027-01-01T00:00:00.000Z"
        ));
        assert_eq!(
            parse_args(os(&["list", "--all"])).unwrap().action,
            TokenAction::List { all: true }
        );
        assert_eq!(
            parse_args(os(&[
                "revoke",
                "3F9C2A71B4E05D18",
                "--reason",
                "離職",
                "--yes"
            ]))
            .unwrap()
            .action,
            TokenAction::Revoke {
                token_id: "3f9c2a71b4e05d18".into(),
                reason: Some("離職".into()),
                yes: true
            }
        );
        assert_eq!(
            parse_args(os(&["--help"])).unwrap().action,
            TokenAction::Help
        );
        for bad in [
            &[][..],
            &["show"],
            &["create"],
            &["create", "--name"],
            &["create", "--name", ""],
            &["create", "--name", "x", "--scope", "write"],
            &["create", "--name", "x", "--expires-in", "0d"],
            &["create", "--name", "x", "--expires-in", "4000d"],
            &[
                "create",
                "--name",
                "x",
                "--expires-in",
                "1d",
                "--expires-at",
                "2099-01-01",
            ],
            &["create", "--name", "x", "--all"],
            &["list", "extra"],
            &["revoke"],
            &["revoke", "kat_3f9c2a71b4e05d18_xxx"],
            &["revoke", "3f9c2a71b4e05d18", "--all"],
        ] {
            assert!(parse_args(os(bad)).is_err(), "{bad:?}");
        }
        assert!(matches!(
            crate::config::parse_args(os(&["api-token", "list"])),
            Ok(crate::config::CliAction::ApiToken(_))
        ));
    }

    #[test]
    fn local_time_and_relative_time() {
        assert_eq!(zone_label(tz8()), "UTC+8");
        assert_eq!(zone_label(FixedOffset::east_opt(0).unwrap()), "UTC");
        assert_eq!(
            zone_label(FixedOffset::west_opt(3 * 3600 + 1800).unwrap()),
            "UTC-3:30"
        );
        // 2026-10-13T06:03:00Z → 台北 14:03
        let ms = 1_760_335_380_000;
        assert_eq!(human_time_zoned(ms, tz8()), "2025-10-13 14:03 UTC+8");
        assert_eq!(relative(ms + 12_000, ms), "12 秒前");
        assert_eq!(relative(ms + 180_000, ms), "3 分鐘前");
        assert_eq!(relative(ms + 8_100_000, ms), "2 小時 15 分前");
        assert_eq!(relative(ms + 14 * 86_400_000, ms), "14 天前");
        // 「最後使用」：7 天內為相對時間，7 天以上改為日期時間（copywriting.md §5.3）
        assert_eq!(last_used_text(ms + 180_000, ms, tz8()), "3 分鐘前");
        assert_eq!(last_used_text(ms + 7 * 86_400_000 - 1, ms, tz8()), "6 天前");
        assert_eq!(
            last_used_text(ms + 7 * 86_400_000, ms, tz8()),
            "2025-10-13 14:03"
        );
        assert_eq!(
            last_used_text(ms + 30 * 86_400_000, ms, tz8()),
            "2025-10-13 14:03"
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kairos-token-cli-{tag}-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_file(dir: &Path) -> PathBuf {
        let path = dir.join("kairos.toml");
        std::fs::write(
            &path,
            format!(
                "[storage]\ndata_dir = \"{}\"\n",
                dir.join("data").to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn create_list_revoke_end_to_end() {
        let dir = temp_dir("e2e");
        let config = config_file(&dir);
        let now = Utc::now();
        let cmd = |action: TokenAction, json: bool| TokenCommand {
            action,
            config_path: Some(config.clone()),
            json,
            ascii: true,
        };
        let mut no = |_: &str| false;
        let mut yes = |_: &str| true;

        // 資料庫還不存在：list 為空、不建立資料庫
        let out = execute(
            &cmd(TokenAction::List { all: false }, false),
            now,
            tz8(),
            false,
            &mut no,
        );
        assert_eq!(out.stdout, "沒有 API token。\n");
        assert!(!dir.join("data").join(DB_FILE).exists());

        // 建立：stdout 只有 token
        let out = execute(
            &cmd(
                TokenAction::Create {
                    name: "grafana".into(),
                    expiry: Expiry::Default,
                    grant: GrantSpec::DefaultViewer,
                },
                false,
            ),
            now,
            tz8(),
            false,
            &mut no,
        );
        assert_eq!(out.exit, 0, "{out:?}");
        let token = out.stdout.trim().to_string();
        let parsed = tokens::parse_token(&token).expect("stdout 應只有 token");
        assert!(
            out.stderr.starts_with("[ OK ] 已建立 API token「grafana」"),
            "{}",
            out.stderr
        );
        assert!(out.stderr.contains(parsed.token_id));
        assert!(!out.stderr.contains(parsed.secret), "說明中不應再出現秘密");
        // 沒有指定 --role：stderr 提示已使用 viewer（cli-style.md §11.6）；stdout 仍只有 token
        assert!(
            out.stderr.contains(
                "[INFO] 未指定 --role，使用唯讀（viewer）；需要讀取註冊、憑證與設定時請改用 --role auditor"
            ),
            "{}",
            out.stderr
        );
        assert!(!out.stdout.contains("viewer"));

        // --json：純 ASCII，含 token
        let out = execute(
            &cmd(
                TokenAction::Create {
                    name: "主控台".into(),
                    expiry: Expiry::In(std::time::Duration::from_secs(10 * 86_400)),
                    grant: GrantSpec::Role(Role::Viewer),
                },
                true,
            ),
            now,
            tz8(),
            false,
            &mut no,
        );
        assert!(out.stdout.is_ascii(), "{}", out.stdout);
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["schema"], "kairos.api-token.create/v1");
        assert_eq!(v["data"]["name"], "主控台");
        // --json 為 UTC
        assert!(
            v["data"]["expires_at"].as_str().unwrap().ends_with('Z'),
            "{v}"
        );
        assert!(v["data"]["token"].as_str().unwrap().starts_with("kat_"));
        assert!(out.stderr.is_empty());

        // 列表：不含秘密
        let out = execute(
            &cmd(TokenAction::List { all: false }, false),
            now,
            tz8(),
            false,
            &mut no,
        );
        assert!(out.stdout.contains(parsed.token_id), "{}", out.stdout);
        assert!(!out.stdout.contains(parsed.secret));
        assert!(
            out.stdout.ends_with("共 2 個（有效 2）\n"),
            "{}",
            out.stdout
        );
        // 本機時區只標在欄位標題；10 天後到期的「主控台」加上到期提示（ASCII 為 [WARN]）
        assert!(out.stdout.starts_with("TOKEN_ID"), "{}", out.stdout);
        assert!(out.stdout.contains("到期（UTC+8）"), "{}", out.stdout);
        let line = |name: &str| {
            out.stdout
                .lines()
                .find(|l| l.contains(name))
                .unwrap_or_default()
                .to_string()
        };
        assert!(line("主控台").ends_with("[WARN]  -"), "{}", out.stdout);
        assert!(!line("grafana").contains("[WARN]"), "{}", out.stdout);
        assert!(
            !out.stdout.contains(" UTC"),
            "儲存格不重複時區：{}",
            out.stdout
        );
        assert!(
            out.stderr.contains("1 個 API token 在 14 天內到期"),
            "{}",
            out.stderr
        );
        let out = execute(
            &cmd(TokenAction::List { all: false }, true),
            now,
            tz8(),
            false,
            &mut no,
        );
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["data"]["tokens"].as_array().unwrap().len(), 2);
        assert!(!out.stdout.contains(parsed.secret));

        // 撤銷：沒有確認時不撤銷（結束代碼 2）
        let revoke = |yes_flag: bool| {
            cmd(
                TokenAction::Revoke {
                    token_id: parsed.token_id.to_string(),
                    reason: Some("測試".into()),
                    yes: yes_flag,
                },
                false,
            )
        };
        let out = execute(&revoke(false), now, tz8(), false, &mut no);
        assert_eq!(out.exit, EXIT_USAGE, "{out:?}");
        assert!(out.stderr.contains("--yes"), "{}", out.stderr);
        let out = execute(&revoke(false), now, tz8(), false, &mut yes);
        assert_eq!(out.exit, 0, "{out:?}");
        assert!(out.stdout.contains("已撤銷"), "{}", out.stdout);
        // 再撤銷一次：不算錯誤
        let out = execute(&revoke(true), now, tz8(), false, &mut no);
        assert_eq!(out.exit, 0);
        assert!(out.stdout.contains("先前已於"), "{}", out.stdout);
        // 預設只列出有效的
        let out = execute(
            &cmd(TokenAction::List { all: false }, false),
            now,
            tz8(),
            false,
            &mut no,
        );
        assert!(!out.stdout.contains(parsed.token_id));
        let out = execute(
            &cmd(TokenAction::List { all: true }, false),
            now,
            tz8(),
            false,
            &mut no,
        );
        assert!(out.stdout.contains("已撤銷"), "{}", out.stdout);
        // 不存在的 token_id
        let out = execute(
            &cmd(
                TokenAction::Revoke {
                    token_id: "ffffffffffffffff".into(),
                    reason: None,
                    yes: true,
                },
                true,
            ),
            now,
            tz8(),
            false,
            &mut no,
        );
        assert_eq!(out.exit, EXIT_GENERAL);
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["ok"], false);
        // 過去的到期日
        let out = execute(
            &cmd(
                TokenAction::Create {
                    name: "x".into(),
                    expiry: Expiry::At(now - chrono::Duration::days(1)),
                    grant: GrantSpec::Role(Role::Viewer),
                },
                false,
            ),
            now,
            tz8(),
            false,
            &mut no,
        );
        assert_eq!(out.exit, EXIT_USAGE);
        // --scope（已棄用）：KRS-W0104；有權限來源時沒有 viewer 的提示
        let legacy = || TokenAction::Create {
            name: "old".into(),
            expiry: Expiry::Default,
            grant: GrantSpec::Legacy(LegacyScope::parse("read").unwrap()),
        };
        let out = execute(&cmd(legacy(), false), now, tz8(), true, &mut no);
        assert_eq!(out.exit, 0, "{out:?}");
        assert!(
            out.stderr.contains("⚠ 警告 KRS-W0104：--scope 已棄用"),
            "{}",
            out.stderr
        );
        assert!(!out.stderr.contains("未指定 --role"), "{}", out.stderr);
        let out = execute(&cmd(legacy(), true), now, tz8(), false, &mut no);
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["warnings"][0]["code"], "KRS-W0104");
        // 設定錯誤：設定檔不存在為 KRS-E0401（與啟動、--check-config 相同）
        let mut bad = cmd(TokenAction::List { all: false }, false);
        bad.config_path = Some(dir.join("missing.toml"));
        let out = execute(&bad, now, tz8(), false, &mut no);
        assert_eq!(out.exit, EXIT_CONFIG);
        assert!(out.stderr.contains("KRS-E0401"), "{}", out.stderr);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
