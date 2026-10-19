//! `kairos users …`：本機帳號、角色指派、工作階段與目錄服務群組對應（`kairos/API.md` 第 12.9 節）。
//!
//! 首位管理員以 `kairos users create <帳號> --role admin` 建立（沒有預設帳號與密碼）：密碼由系統產生、
//! 只在 stdout 顯示一次（`PW=$(kairos users create …)`），第一次登入時必須變更；或以 `--password-stdin`
//! 從標準輸入讀取。輸出依設計包 `cli-style.md`：結果在 stdout、過程與錯誤在 stderr；`--json` 為純 ASCII 的
//! 單一文件（`kairos.users.<子命令>/v1`）。危險操作依 §5.4：停用、重設、結束工作階段為 L1（互動詢問，
//! 非互動需要 `--yes`），刪除為 L2（互動時輸入帳號）。每個變更都寫入稽核雜湊鏈，執行者為作業系統帳號。

use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, ErrorKind, IsTerminal, Write};
use std::path::PathBuf;

use crate::audit::Event;
use crate::config::{AppConfig, take_config_arg};
use crate::hosts_cli::{ascii_json, use_unicode};
use crate::rbac::{Assignment, Role};
use crate::storage::fmt_time;
use crate::store::db::{StoreErrorKind, in_write_tx};
use crate::store::prelude::*;
use crate::token_cli::{Failure, db_failure, human_time, open, operator, table, zone_label};
use crate::users::password::{self, HashParams};
use crate::users::store::{
    self as us, NewUser, SOURCE_LOCAL, SUBJECT_LDAP_GROUP, SUBJECT_USER, UserRow,
};

/// `kairos users --help`
pub const USERS_USAGE: &str = "kairos users — 主控台的使用者、角色與工作階段（kairos/API.md 第 12 節）

用法：
  kairos users create <帳號> --role <角色> [--role …] [--display-name <名稱>] [--email <Email>] [--password-stdin]
  kairos users list [--all]
  kairos users show <帳號>
  kairos users grant <帳號> --role <角色>
  kairos users revoke-role <帳號> --role <角色> [--yes]
  kairos users disable <帳號> [--reason <原因>] [--yes]
  kairos users enable <帳號>
  kairos users delete <帳號> [--yes]
  kairos users reset-password <帳號> [--password-stdin] [--yes]
  kairos users reset-mfa <帳號> [--reason <原因>] [--yes]
  kairos users unlock <帳號>
  kairos users sessions <帳號>
  kairos users end-sessions <帳號> [--reason <原因>] [--yes]
  kairos users groups
  kairos users map-group <群組 DN> --role <角色>
  kairos users unmap-group <群組 DN> --role <角色>

角色：admin（管理員）、operator（維運者）、auditor（稽核員）、viewer（唯讀）；
限定主機群組時寫成 operator@group:<群組名稱>（ia.md §4.3）。

首位管理員（沒有預設帳號與密碼）：
  sudo -u kairos /opt/kairos/kairos users create admin --role admin --config /opt/kairos/kairos.toml
  產生的一次性密碼只顯示在 stdout 這一次，第一次登入時必須變更。要自行指定密碼時加上 --password-stdin
  並從標準輸入提供（至少 accounts.password_min_length 個字元，預設 12）。

通用選項：
  --config <path>        Kairos 的設定檔（用來找到 storage.data_dir）
  --json                 輸出機器可讀的 JSON（純 ASCII）
  --ascii                不使用 Unicode 符號
  -h, --help             顯示本說明

disable、revoke-role、reset-password、reset-mfa、end-sessions 為 L1（互動詢問，非互動需要 --yes）；
delete 為 L2（互動時輸入帳號，非互動需要 --yes）。不能移除、停用或刪除最後一位全站管理員。

結束代碼：0 完成、1 資料庫錯誤或找不到使用者、2 用法錯誤、3 設定錯誤、6 被拒絕（最後一位管理員、
目錄服務帳號）、10 權限不足、11 衝突（帳號已存在、資料庫使用中）";

const EXIT_GENERAL: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_REFUSED: u8 = 6;
const EXIT_CONFLICT: u8 = 11;

/// 子命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsersAction {
    Help,
    Create {
        username: String,
        roles: Vec<Assignment>,
        display_name: Option<String>,
        email: Option<String>,
        password_stdin: bool,
    },
    List {
        all: bool,
    },
    Show(String),
    Grant(String, Assignment),
    RevokeRole(String, Assignment),
    Disable(String),
    Enable(String),
    Delete(String),
    ResetPassword(String),
    ResetMfa(String),
    Unlock(String),
    Sessions(String),
    EndSessions(String),
    Groups,
    MapGroup(String, Assignment),
    UnmapGroup(String, Assignment),
}

/// `kairos users` 的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsersCommand {
    pub action: UsersAction,
    pub config_path: Option<PathBuf>,
    pub json: bool,
    pub ascii: bool,
    pub yes: bool,
    pub reason: Option<String>,
    pub password_stdin: bool,
}

const SUBCOMMANDS: [&str; 16] = [
    "create",
    "list",
    "show",
    "grant",
    "revoke-role",
    "disable",
    "enable",
    "delete",
    "reset-password",
    "reset-mfa",
    "unlock",
    "sessions",
    "end-sessions",
    "groups",
    "map-group",
    "unmap-group",
];

/// 解析 `kairos users` 之後的參數
pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<UsersCommand, String> {
    let mut args = args.peekable();
    let help = |config_path| UsersCommand {
        action: UsersAction::Help,
        config_path,
        json: false,
        ascii: false,
        yes: false,
        reason: None,
        password_stdin: false,
    };
    let sub = match args.next() {
        None => {
            return Err(format!(
                "kairos users 需要子命令（{}）",
                SUBCOMMANDS.join("、")
            ));
        }
        Some(a) if a == "-h" || a == "--help" => return Ok(help(None)),
        Some(a) => a.to_string_lossy().into_owned(),
    };
    if !SUBCOMMANDS.contains(&sub.as_str()) {
        return Err(format!(
            "無法辨識的子命令: {sub}（{}）",
            SUBCOMMANDS.join("、")
        ));
    }
    let mut config_path = None;
    let (mut json, mut ascii, mut yes, mut all, mut stdin) = (false, false, false, false, false);
    let (mut roles, mut display, mut email, mut reason) = (Vec::new(), None, None, None);
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
        match a.split('=').next().unwrap_or("") {
            "-h" | "--help" => return Ok(help(config_path)),
            "--json" => json = true,
            "--ascii" => ascii = true,
            "--yes" => yes = true,
            "--all" if sub == "list" => all = true,
            "--password-stdin" if matches!(sub.as_str(), "create" | "reset-password") => {
                stdin = true
            }
            "--role"
                if matches!(
                    sub.as_str(),
                    "create" | "grant" | "revoke-role" | "map-group" | "unmap-group"
                ) =>
            {
                roles.push(Assignment::parse(&value("--role")?)?);
            }
            "--display-name" if sub == "create" => display = Some(value("--display-name")?),
            "--email" if sub == "create" => email = Some(value("--email")?),
            "--reason" => reason = Some(value("--reason")?),
            _ if a.starts_with('-') => return Err(format!("無法辨識的參數: {a}")),
            _ => positional.push(a.clone()),
        }
    }
    let one = |what: &str| -> Result<String, String> {
        match positional.as_slice() {
            [x] => Ok(x.clone()),
            [] => Err(format!("kairos users {sub} 需要{what}")),
            _ => Err(format!("無法辨識的參數: {}", positional[1])),
        }
    };
    let one_role = |roles: &[Assignment]| -> Result<Assignment, String> {
        match roles {
            [r] => Ok(r.clone()),
            _ => Err(format!("kairos users {sub} 需要恰好一個 --role")),
        }
    };
    let action = match sub.as_str() {
        "create" => {
            let username = one("帳號")?;
            us::validate_username(&username).map_err(|e| format!("帳號無效：{e}"))?;
            if roles.is_empty() {
                return Err(
                    "kairos users create 需要至少一個 --role（例如 --role admin）".to_string(),
                );
            }
            for (k, v) in [("--display-name", &display), ("--email", &email)] {
                if let Some(v) = v {
                    us::validate_text(k, v)?;
                }
            }
            UsersAction::Create {
                username,
                roles: roles.clone(),
                display_name: display,
                email,
                password_stdin: stdin,
            }
        }
        "list" => {
            if !positional.is_empty() {
                return Err(format!("無法辨識的參數: {}", positional[0]));
            }
            UsersAction::List { all }
        }
        "show" => UsersAction::Show(one("帳號")?),
        "grant" => UsersAction::Grant(one("帳號")?, one_role(&roles)?),
        "revoke-role" => UsersAction::RevokeRole(one("帳號")?, one_role(&roles)?),
        "disable" => UsersAction::Disable(one("帳號")?),
        "enable" => UsersAction::Enable(one("帳號")?),
        "delete" => UsersAction::Delete(one("帳號")?),
        "reset-password" => UsersAction::ResetPassword(one("帳號")?),
        "reset-mfa" => UsersAction::ResetMfa(one("帳號")?),
        "unlock" => UsersAction::Unlock(one("帳號")?),
        "sessions" => UsersAction::Sessions(one("帳號")?),
        "end-sessions" => UsersAction::EndSessions(one("帳號")?),
        "groups" => UsersAction::Groups,
        "map-group" => UsersAction::MapGroup(one("群組 DN")?, one_role(&roles)?),
        _ => UsersAction::UnmapGroup(one("群組 DN")?, one_role(&roles)?),
    };
    Ok(UsersCommand {
        action,
        config_path,
        json,
        ascii,
        yes,
        reason,
        password_stdin: stdin,
    })
}

/// 輸出（stdout、stderr）與結束代碼
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit: u8,
}

/// 互動（測試可替換）：詢問並取得一行輸入；讀取 `--password-stdin` 的密碼
pub trait Io {
    /// 互動詢問；非互動時回 `None`
    fn ask(&mut self, prompt: &str) -> Option<String>;
    /// 從標準輸入讀取一行（`--password-stdin`）
    fn read_secret(&mut self) -> Option<String>;
}

fn schema(a: &UsersAction) -> String {
    let name = match a {
        UsersAction::Help | UsersAction::List { .. } => "list",
        UsersAction::Create { .. } => "create",
        UsersAction::Show(_) => "show",
        UsersAction::Grant(..) => "grant",
        UsersAction::RevokeRole(..) => "revoke-role",
        UsersAction::Disable(_) => "disable",
        UsersAction::Enable(_) => "enable",
        UsersAction::Delete(_) => "delete",
        UsersAction::ResetPassword(_) => "reset-password",
        UsersAction::ResetMfa(_) => "reset-mfa",
        UsersAction::Unlock(_) => "unlock",
        UsersAction::Sessions(_) => "sessions",
        UsersAction::EndSessions(_) => "end-sessions",
        UsersAction::Groups => "groups",
        UsersAction::MapGroup(..) => "map-group",
        UsersAction::UnmapGroup(..) => "unmap-group",
    };
    format!("kairos.users.{name}/v1")
}

fn not_found(name: &str) -> Failure {
    Failure::new(
        format!("找不到使用者 {name}"),
        "資料庫中沒有這個帳號",
        vec!["以 kairos users list --all 確認帳號。".to_string()],
        EXIT_GENERAL,
    )
}

fn last_admin() -> Failure {
    Failure::new(
        "不能移除最後一位全站管理員",
        "這是唯一一位啟用中、具有全站管理員角色的使用者；移除後沒有人能管理使用者與系統設定",
        vec!["先以 kairos users create 或 grant 指派另一位管理員，再執行這個操作。".to_string()],
        EXIT_REFUSED,
    )
}

fn user_json(u: &UserRow, roles: &[Assignment], now_ms: i64) -> Value {
    let mut v = u.json(now_ms);
    v["roles"] = json!(roles.iter().map(Assignment::json).collect::<Vec<_>>());
    v
}

fn roles_text(roles: &[Assignment]) -> String {
    if roles.is_empty() {
        "—".to_string()
    } else {
        roles
            .iter()
            .map(|a| match &a.scope {
                crate::rbac::Scope::Global => format!("{}（全站）", a.role.label()),
                crate::rbac::Scope::Group(g) => format!("{}（{g}）", a.role.label()),
            })
            .collect::<Vec<_>>()
            .join("、")
    }
}

fn status_label(s: &str) -> &'static str {
    match s {
        "disabled" => "停用",
        "locked" => "鎖定",
        _ => "啟用",
    }
}

/// 執行命令（不直接讀寫 stdin／stdout，方便測試）
pub fn execute(
    cmd: &UsersCommand,
    now: DateTime<Utc>,
    tz: FixedOffset,
    unicode: bool,
    io: &mut dyn Io,
) -> Output {
    let schema = schema(&cmd.action);
    let fail = |f: Failure| Output {
        stdout: if cmd.json {
            f.json(&schema, now)
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
    let ok_json = |data: Value| Output {
        stdout: ascii_json(&json!({
            "schema": schema, "ok": true, "generated_at": fmt_time(now),
            "data": data, "warnings": [], "error": null,
        })) + "\n",
        ..Default::default()
    };
    let mark = if unicode { "✓" } else { "[ OK ]" };
    if cmd.action == UsersAction::Help {
        return Output {
            stdout: format!("{USERS_USAGE}\n"),
            ..Default::default()
        };
    }
    let creating = matches!(cmd.action, UsersAction::Create { .. });
    let conn = match open(cmd.config_path.as_deref(), creating) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return match &cmd.action {
                UsersAction::List { .. } | UsersAction::Groups => {
                    if cmd.json {
                        ok_json(json!({ "users": [], "mappings": [] }))
                    } else {
                        Output {
                            stdout: "沒有使用者。\n".to_string(),
                            ..Default::default()
                        }
                    }
                }
                _ => fail(db_failure("資料庫不存在（還沒有任何使用者）")),
            };
        }
        Err(f) => return fail(f),
    };
    let config = AppConfig::load(cmd.config_path.as_deref()).unwrap_or_default();
    let by = operator();
    let now_ms = now.timestamp_millis();
    let find = |name: &str| -> Result<UserRow, Failure> {
        conn.find_user(name)
            .map_err(db_failure)?
            .ok_or_else(|| not_found(name))
    };
    // L1：互動詢問 y/N；非互動需要 --yes
    let mut confirm_l1 = |prompt: &str, retry: &str| -> Result<(), Failure> {
        if cmd.yes {
            return Ok(());
        }
        match io.ask(&format!("{prompt}\n是否繼續？[y/N] ")) {
            Some(a) if matches!(a.trim(), "y" | "Y" | "yes" | "YES") => Ok(()),
            _ => Err(Failure::new(
                "未執行",
                "沒有確認（非互動執行時需要 --yes）",
                vec![format!("確認要執行時加上 --yes：{retry} --yes")],
                EXIT_USAGE,
            )),
        }
    };
    let audit = |conn: &dyn Store, action: &'static str, object: &str, details: Value| {
        Event {
            actor: by.clone(),
            actor_ip: None,
            category: "user",
            action,
            object: Some(object.to_string()),
            result: "ok",
            code: None,
            details: Some(details),
        }
        .append(conn, now_ms)
    };
    let acc = &config.accounts;
    match &cmd.action {
        UsersAction::Help => unreachable!(),
        UsersAction::Create {
            username,
            roles,
            display_name,
            email,
            password_stdin,
        } => {
            let (pw, generated) = if *password_stdin {
                let Some(pw) = io.read_secret() else {
                    return fail(Failure::new(
                        "沒有讀到密碼",
                        "--password-stdin 需要從標準輸入提供一行密碼",
                        vec![
                            "例如：printf '%s\\n' \"$PW\" | kairos users create … --password-stdin"
                                .to_string(),
                        ],
                        EXIT_USAGE,
                    ));
                };
                if let Err(e) = password::check_policy(&pw, username, acc.password_min_length) {
                    return fail(Failure::new(
                        "密碼不符合政策",
                        e,
                        vec![format!(
                            "密碼至少 {} 個字元，不可包含帳號、不可是常見的密碼。",
                            acc.password_min_length
                        )],
                        EXIT_USAGE,
                    ));
                }
                (pw, false)
            } else {
                match password::generate() {
                    Ok(p) => (p, true),
                    Err(e) => return fail(db_failure(e)),
                }
            };
            let phc = match password::hash(&pw, HashParams::from(acc)) {
                Ok(h) => h,
                Err(e) => return fail(db_failure(e)),
            };
            let r = in_write_tx(&*conn, |c| -> StoreResult<_> {
                let Some(uid) = c.create_user(
                    &NewUser {
                        username,
                        display_name: display_name.as_deref(),
                        email: email.as_deref(),
                        source: SOURCE_LOCAL,
                        password_hash: Some(&phc),
                        must_change_password: generated,
                        ..NewUser::default()
                    },
                    &by,
                    now_ms,
                )?
                else {
                    return Ok(None);
                };
                for a in roles {
                    c.add_assignment(SUBJECT_USER, &uid.to_string(), a, &by, now_ms)?;
                }
                audit(
                    c,
                    "user.create",
                    username,
                    json!({
                        "roles": roles.iter().map(Assignment::as_text).collect::<Vec<_>>(),
                        "password": if generated { "generated" } else { "provided" },
                        "via": "cli",
                    }),
                )?;
                c.get_user(uid)
            });
            let u = match r {
                Ok(Some(u)) => u,
                Ok(None) => {
                    return fail(Failure::new(
                        format!("使用者 {username} 已存在"),
                        "帳號不分大小寫，同名的帳號已存在",
                        vec![format!(
                            "改用其他帳號，或以 kairos users show {username} 查看。"
                        )],
                        EXIT_CONFLICT,
                    ));
                }
                Err(e) => return fail(db_failure(e)),
            };
            if cmd.json {
                let mut data = user_json(&u, roles, now_ms);
                if generated {
                    data["initial_password"] = json!(pw);
                }
                return ok_json(data);
            }
            Output {
                stdout: if generated {
                    format!("{pw}\n")
                } else {
                    String::new()
                },
                stderr: format!(
                    "{mark} 已建立使用者「{}」（{}）\n{}",
                    u.username,
                    roles_text(roles),
                    if generated {
                        "  上一行是一次性的初始密碼，只顯示這一次；第一次登入主控台時必須變更。\n"
                    } else {
                        ""
                    }
                ),
                exit: 0,
            }
        }
        UsersAction::List { all } => {
            let users = match conn.list_users() {
                Ok(u) => u,
                Err(e) => return fail(db_failure(e)),
            };
            let mut rows = Vec::new();
            for u in users.into_iter().filter(|u| *all || u.active()) {
                let roles = conn.user_assignments(u.user_id).unwrap_or_default();
                rows.push((u, roles));
            }
            if cmd.json {
                return ok_json(
                    json!({ "users": rows.iter().map(|(u, r)| user_json(u, r, now_ms)).collect::<Vec<_>>() }),
                );
            }
            if rows.is_empty() {
                return Output {
                    stdout: "沒有使用者（首位管理員：kairos users create <帳號> --role admin）。\n"
                        .to_string(),
                    ..Default::default()
                };
            }
            let zone = zone_label(tz);
            let mut t = vec![vec![
                "帳號".to_string(),
                "名稱".to_string(),
                "來源".to_string(),
                "角色（範圍）".to_string(),
                "MFA".to_string(),
                format!("最後登入（{zone}）"),
                "狀態".to_string(),
            ]];
            for (u, r) in &rows {
                t.push(vec![
                    u.username.clone(),
                    u.display_name.clone().unwrap_or_else(|| "—".into()),
                    if u.source == SOURCE_LOCAL {
                        "本機".into()
                    } else {
                        "AD".into()
                    },
                    roles_text(r),
                    if u.totp_enabled_at.is_some() {
                        "TOTP".into()
                    } else if u.source == SOURCE_LOCAL {
                        "—".into()
                    } else {
                        "AD".into()
                    },
                    u.last_login_at
                        .map_or_else(|| "—".into(), |t| human_time(t, tz)),
                    status_label(u.status(now_ms)).to_string(),
                ]);
            }
            Output {
                stdout: format!("{}\n共 {} 位\n", table(&t), rows.len()),
                ..Default::default()
            }
        }
        UsersAction::Show(name) => {
            let u = match find(name) {
                Ok(u) => u,
                Err(f) => return fail(f),
            };
            let roles = conn.user_assignments(u.user_id).unwrap_or_default();
            let sessions = conn
                .user_sessions(u.user_id, true, now_ms)
                .unwrap_or_default();
            if cmd.json {
                let mut v = user_json(&u, &roles, now_ms);
                v["sessions"] = json!(
                    sessions
                        .iter()
                        .map(|s| s.json(now_ms, false))
                        .collect::<Vec<_>>()
                );
                return ok_json(v);
            }
            let mut out = format!(
                "帳號：{}\n名稱：{}\n來源：{}\n狀態：{}\n角色：{}\nMFA：{}\n有效的工作階段：{} 個\n",
                u.username,
                u.display_name.as_deref().unwrap_or("—"),
                if u.source == SOURCE_LOCAL {
                    "本機"
                } else {
                    "目錄服務（AD）"
                },
                status_label(u.status(now_ms)),
                roles_text(&roles),
                if u.totp_enabled_at.is_some() {
                    "TOTP"
                } else {
                    "未啟用"
                },
                sessions.len()
            );
            if u.must_change_password {
                out.push_str("下次登入時必須變更密碼\n");
            }
            Output {
                stdout: out,
                ..Default::default()
            }
        }
        UsersAction::Grant(name, a) | UsersAction::RevokeRole(name, a) => {
            let grant = matches!(cmd.action, UsersAction::Grant(..));
            let u = match find(name) {
                Ok(u) => u,
                Err(f) => return fail(f),
            };
            if !grant
                && let Err(f) = confirm_l1(
                    &format!(
                        "即將移除「{}」的角色 {}（權限立即生效，已登入的工作階段在下一個請求時套用）",
                        u.username,
                        a.as_text()
                    ),
                    &format!("kairos users revoke-role {name} --role {}", a.as_text()),
                )
            {
                return fail(f);
            }
            let r = in_write_tx(&*conn, |c| -> StoreResult<_> {
                if !grant
                    && a.role == Role::Admin
                    && a.scope == crate::rbac::Scope::Global
                    && c.active_global_admins()? == vec![u.user_id]
                {
                    return Ok(Err(()));
                }
                let changed = if grant {
                    c.add_assignment(SUBJECT_USER, &u.user_id.to_string(), a, &by, now_ms)?
                } else {
                    c.remove_assignment(SUBJECT_USER, &u.user_id.to_string(), a)?
                };
                if changed {
                    let key = if grant { "added" } else { "removed" };
                    audit(
                        c,
                        "role.change",
                        &u.username,
                        json!({ key: [a.as_text()], "reason": cmd.reason, "via": "cli" }),
                    )?;
                }
                Ok(Ok(changed))
            });
            let changed = match r {
                Ok(Ok(c)) => c,
                Ok(Err(())) => return fail(last_admin()),
                Err(e) => return fail(db_failure(e)),
            };
            if cmd.json {
                return ok_json(
                    json!({ "username": u.username, "role": a.json(), "changed": changed }),
                );
            }
            let verb = if grant { "指派" } else { "移除" };
            Output {
                stdout: if changed {
                    format!("{mark} 已{verb}「{}」的角色 {}\n", u.username, a.as_text())
                } else {
                    format!(
                        "{mark} 「{}」{}角色 {}（沒有變更）\n",
                        u.username,
                        if grant { "已有" } else { "沒有" },
                        a.as_text()
                    )
                },
                ..Default::default()
            }
        }
        UsersAction::Disable(name)
        | UsersAction::Enable(name)
        | UsersAction::Delete(name)
        | UsersAction::Unlock(name)
        | UsersAction::ResetMfa(name)
        | UsersAction::EndSessions(name)
        | UsersAction::ResetPassword(name) => {
            let u = match find(name) {
                Ok(u) => u,
                Err(f) => return fail(f),
            };
            let action = &cmd.action;
            // 確認
            let confirmed = match action {
                UsersAction::Disable(_) => confirm_l1(
                    &format!(
                        "即將停用「{}」\n影響：\n  • 所有工作階段立即結束、無法再登入；屬於這個帳號的 API token 立即失效。\n  • 可以再以 kairos users enable 啟用。",
                        u.username
                    ),
                    &format!("kairos users disable {name}"),
                ),
                UsersAction::ResetMfa(_) => confirm_l1(
                    &format!(
                        "即將重設「{}」的兩步驟驗證（TOTP 與復原碼一併刪除，下次登入只需要密碼，之後可重新綁定）",
                        u.username
                    ),
                    &format!("kairos users reset-mfa {name}"),
                ),
                UsersAction::EndSessions(_) => confirm_l1(
                    &format!(
                        "即將結束「{}」的所有工作階段\n  對方在所有裝置上的工作階段都會立即結束，需要重新登入。帳號不會被停用。",
                        u.username
                    ),
                    &format!("kairos users end-sessions {name}"),
                ),
                UsersAction::ResetPassword(_) => {
                    if u.source != SOURCE_LOCAL {
                        return fail(Failure::new(
                            "不能重設目錄服務帳號的密碼",
                            "這個帳號由目錄服務（AD）管理",
                            vec!["請網域管理員在 AD 重設密碼。".to_string()],
                            EXIT_REFUSED,
                        ));
                    }
                    confirm_l1(
                        &format!(
                            "即將重設「{}」的密碼（所有工作階段結束，下次登入時必須變更）",
                            u.username
                        ),
                        &format!("kairos users reset-password {name}"),
                    )
                }
                UsersAction::Delete(_) => {
                    if cmd.yes {
                        Ok(())
                    } else {
                        match io.ask(&format!(
                            "即將刪除使用者「{}」\n影響：\n  • 角色指派、兩步驟驗證一併刪除，所有工作階段結束，擁有的 API token 撤銷。\n  • 稽核紀錄保留。這個操作無法復原。\n請輸入帳號「{}」以確認（按 Ctrl+C 取消）：",
                            u.username, u.username
                        )) {
                            Some(a) if a.trim().eq_ignore_ascii_case(&u.username) => Ok(()),
                            _ => Err(Failure::new("未刪除", "輸入的帳號不符，或沒有確認（非互動執行時需要 --yes）", vec![format!("確認要刪除時：kairos users delete {name} --yes")], EXIT_USAGE)),
                        }
                    }
                }
                _ => Ok(()),
            };
            if let Err(f) = confirmed {
                return fail(f);
            }
            let new_pw = if matches!(action, UsersAction::ResetPassword(_)) {
                let pw = if cmd.password_stdin {
                    match io.read_secret() {
                        Some(pw) => {
                            if let Err(e) =
                                password::check_policy(&pw, &u.username, acc.password_min_length)
                            {
                                return fail(Failure::new("密碼不符合政策", e, vec![], EXIT_USAGE));
                            }
                            (pw, false)
                        }
                        None => {
                            return fail(Failure::new(
                                "沒有讀到密碼",
                                "--password-stdin 需要從標準輸入提供一行密碼",
                                vec![],
                                EXIT_USAGE,
                            ));
                        }
                    }
                } else {
                    match password::generate() {
                        Ok(p) => (p, true),
                        Err(e) => return fail(db_failure(e)),
                    }
                };
                match password::hash(&pw.0, HashParams::from(acc)) {
                    Ok(h) => Some((pw.0, pw.1, h)),
                    Err(e) => return fail(db_failure(e)),
                }
            } else {
                None
            };
            let uid = u.user_id;
            let r = in_write_tx(&*conn, |c| -> StoreResult<_> {
                if matches!(action, UsersAction::Disable(_) | UsersAction::Delete(_))
                    && c.active_global_admins()? == vec![uid]
                {
                    return Ok(Err(()));
                }
                let (act, changed, ended): (&'static str, bool, usize) = match action {
                    UsersAction::Disable(_) => {
                        let ch = c.set_state(uid, false, &by, cmd.reason.as_deref(), now_ms)?;
                        (
                            "user.disable",
                            ch,
                            c.end_user_sessions(uid, None, "user_disabled", &by, now_ms)?,
                        )
                    }
                    UsersAction::Enable(_) => {
                        ("user.enable", c.set_state(uid, true, &by, None, now_ms)?, 0)
                    }
                    UsersAction::Delete(_) => {
                        let n = c.user_sessions(uid, true, now_ms)?.len();
                        ("user.delete", c.delete_user(uid, &by, now_ms)?, n)
                    }
                    UsersAction::Unlock(_) => ("user.unlock", c.unlock(uid)?, 0),
                    UsersAction::ResetMfa(_) => ("mfa.reset", c.clear_totp(uid, now_ms)?, 0),
                    UsersAction::EndSessions(_) => {
                        let n = c.end_user_sessions(uid, None, "revoked_by_admin", &by, now_ms)?;
                        ("session.revoke_all", true, n)
                    }
                    _ => {
                        let (_, _, h) = new_pw.as_ref().ok_or_else(|| {
                            StoreError::new(StoreErrorKind::Other, "Query is not read-only")
                        })?;
                        c.set_password(uid, h, true, now_ms)?;
                        (
                            "password.reset",
                            true,
                            c.end_user_sessions(uid, None, "password_reset", &by, now_ms)?,
                        )
                    }
                };
                if changed {
                    audit(
                        c,
                        act,
                        &u.username,
                        json!({ "reason": cmd.reason, "sessions_ended": ended, "via": "cli" }),
                    )?;
                }
                Ok(Ok((changed, ended)))
            });
            let (changed, ended) = match r {
                Ok(Ok(x)) => x,
                Ok(Err(())) => return fail(last_admin()),
                Err(e) => return fail(db_failure(e)),
            };
            let generated_pw = new_pw
                .as_ref()
                .filter(|(_, g, _)| *g)
                .map(|(p, _, _)| p.clone());
            if cmd.json {
                let mut v =
                    json!({ "username": u.username, "changed": changed, "sessions_ended": ended });
                if let Some(p) = &generated_pw {
                    v["password"] = json!(p);
                }
                return ok_json(v);
            }
            let what = match action {
                UsersAction::Disable(_) => {
                    format!("已停用「{}」，結束 {ended} 個工作階段", u.username)
                }
                UsersAction::Enable(_) => format!("已啟用「{}」", u.username),
                UsersAction::Delete(_) => format!("已刪除「{}」", u.username),
                UsersAction::Unlock(_) => format!("已解除「{}」的鎖定", u.username),
                UsersAction::ResetMfa(_) => format!("已重設「{}」的兩步驟驗證", u.username),
                UsersAction::EndSessions(_) => {
                    format!("已結束 {} 的所有工作階段（{ended} 個）", u.username)
                }
                _ => format!(
                    "已重設「{}」的密碼，結束 {ended} 個工作階段；下次登入時必須變更",
                    u.username
                ),
            };
            let what = if changed {
                what
            } else {
                format!("「{}」沒有變更", u.username)
            };
            match generated_pw {
                Some(p) => Output {
                    stdout: format!("{p}\n"),
                    stderr: format!("{mark} {what}\n  上一行是一次性的新密碼，只顯示這一次。\n"),
                    exit: 0,
                },
                None => Output {
                    stdout: format!("{mark} {what}\n"),
                    ..Default::default()
                },
            }
        }
        UsersAction::Sessions(name) => {
            let u = match find(name) {
                Ok(u) => u,
                Err(f) => return fail(f),
            };
            let list = conn
                .user_sessions(u.user_id, true, now_ms)
                .unwrap_or_default();
            if cmd.json {
                return ok_json(
                    json!({ "username": u.username, "sessions": list.iter().map(|s| s.json(now_ms, false)).collect::<Vec<_>>() }),
                );
            }
            if list.is_empty() {
                return Output {
                    stdout: format!("{} 沒有有效的工作階段。\n", u.username),
                    ..Default::default()
                };
            }
            let zone = zone_label(tz);
            let mut t = vec![vec![
                "SESSION_ID".to_string(),
                format!("登入（{zone}）"),
                format!("最後活動（{zone}）"),
                "來源 IP".to_string(),
                "方式".to_string(),
            ]];
            for s in &list {
                t.push(vec![
                    s.public_id.clone(),
                    human_time(s.created_at, tz),
                    human_time(s.last_seen_at, tz),
                    s.ip.clone().unwrap_or_default(),
                    s.method.clone(),
                ]);
            }
            Output {
                stdout: table(&t),
                ..Default::default()
            }
        }
        UsersAction::Groups => {
            let list = conn.ldap_mappings().unwrap_or_default();
            if cmd.json {
                return ok_json(
                    json!({ "mappings": list.iter().map(|(g, a)| { let mut j = a.json(); j["group_dn"] = json!(g); j }).collect::<Vec<_>>() }),
                );
            }
            if list.is_empty() {
                return Output {
                    stdout: "沒有目錄服務群組的對應。\n".to_string(),
                    ..Default::default()
                };
            }
            let mut t = vec![vec!["群組 DN".to_string(), "角色（範圍）".to_string()]];
            for (g, a) in &list {
                t.push(vec![g.clone(), roles_text(std::slice::from_ref(a))]);
            }
            Output {
                stdout: table(&t),
                ..Default::default()
            }
        }
        UsersAction::MapGroup(dn, a) | UsersAction::UnmapGroup(dn, a) => {
            let map = matches!(cmd.action, UsersAction::MapGroup(..));
            let key = us::normalize_dn(dn);
            if key.is_empty() || key.chars().any(char::is_control) || key.len() > 1024 {
                return fail(Failure::new(
                    "群組 DN 無效",
                    "1～1024 個字元，不含控制字元",
                    vec![],
                    EXIT_USAGE,
                ));
            }
            let r = in_write_tx(&*conn, |c| -> StoreResult<_> {
                let changed = if map {
                    c.add_assignment(SUBJECT_LDAP_GROUP, &key, a, &by, now_ms)?
                } else {
                    c.remove_assignment(SUBJECT_LDAP_GROUP, &key, a)?
                };
                if changed {
                    let k = if map { "added" } else { "removed" };
                    audit(
                        c,
                        "directory.mappings.change",
                        "ldap",
                        json!({ k: [format!("{key} → {}", a.as_text())], "via": "cli" }),
                    )?;
                }
                Ok(changed)
            });
            match r {
                Ok(changed) if cmd.json => {
                    ok_json(json!({ "group_dn": key, "role": a.json(), "changed": changed }))
                }
                Ok(changed) => Output {
                    stdout: format!(
                        "{mark} {}「{key}」→ {}{}\n",
                        if map { "已對應" } else { "已移除對應" },
                        a.as_text(),
                        if changed { "" } else { "（沒有變更）" }
                    ),
                    ..Default::default()
                },
                Err(e) => fail(db_failure(e)),
            }
        }
    }
}

/// 終端機的互動
struct Terminal {
    interactive: bool,
}

impl Io for Terminal {
    fn ask(&mut self, prompt: &str) -> Option<String> {
        if !self.interactive {
            return None;
        }
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).ok()?;
        Some(line)
    }

    fn read_secret(&mut self) -> Option<String> {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).ok()?;
        let pw = line.trim_end_matches(['\r', '\n']).to_string();
        (!pw.is_empty()).then_some(pw)
    }
}

/// 執行 `kairos users …`，回傳結束代碼
pub fn run(cmd: UsersCommand) -> u8 {
    let unicode = !cmd.json && use_unicode(cmd.ascii);
    if cmd.password_stdin && std::io::stdin().is_terminal() {
        eprint!(
            "{}",
            Failure::new(
                "--password-stdin 需要從管線或檔案提供密碼",
                "標準輸入是終端機：輸入的密碼會顯示在畫面上",
                vec![
                    "例如：kairos users create admin --role admin --password-stdin < 密碼檔（檔案權限 0600，用完刪除）".to_string(),
                    "或不加 --password-stdin，由系統產生一次性密碼（第一次登入時必須變更）。".to_string(),
                ],
                EXIT_USAGE,
            )
            .human(unicode)
        );
        return EXIT_USAGE;
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut io = Terminal { interactive };
    let out = execute(
        &cmd,
        Utc::now(),
        Local::now().offset().fix(),
        unicode,
        &mut io,
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

    /// 測試用的互動：依序回答
    pub(crate) struct Script {
        pub answers: Vec<String>,
        pub secret: Option<String>,
    }

    impl Io for Script {
        fn ask(&mut self, _: &str) -> Option<String> {
            (!self.answers.is_empty()).then(|| self.answers.remove(0))
        }
        fn read_secret(&mut self) -> Option<String> {
            self.secret.take()
        }
    }

    fn tz8() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).unwrap()
    }

    fn config_file(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "kairos-users-cli-{tag}-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("data");
        let cfg = dir.join("kairos.toml");
        std::fs::write(
            &cfg,
            format!(
                "[storage]\ndata_dir = \"{}\"\n[accounts]\nargon2_memory_kib = 8192\nargon2_iterations = 1\n",
                data.display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        (dir, cfg)
    }

    fn run_cmd(args: &[&str], cfg: &std::path::Path, io: &mut Script) -> Output {
        let mut all: Vec<&str> = args.to_vec();
        let c = cfg.to_string_lossy().into_owned();
        all.push("--config");
        all.push(&c);
        let cmd = parse_args(os(&all)).unwrap();
        execute(&cmd, Utc::now(), tz8(), true, io)
    }

    #[test]
    fn argument_parsing() {
        let c = parse_args(os(&[
            "create",
            "admin",
            "--role",
            "admin",
            "--role=operator@group:Line3",
        ]))
        .unwrap();
        let UsersAction::Create {
            roles,
            password_stdin,
            ..
        } = &c.action
        else {
            panic!()
        };
        assert_eq!(roles.len(), 2);
        assert!(!password_stdin);
        assert!(parse_args(os(&["create", "admin"])).is_err(), "需要 --role");
        assert!(parse_args(os(&["create", "-bad", "--role", "admin"])).is_err());
        assert!(parse_args(os(&["create", "a", "--role", "root"])).is_err());
        assert!(parse_args(os(&["grant", "a"])).is_err());
        assert!(parse_args(os(&["frobnicate"])).is_err());
        assert_eq!(
            parse_args(os(&["disable", "a", "--yes", "--reason", "離職"]))
                .unwrap()
                .reason
                .as_deref(),
            Some("離職")
        );
        assert_eq!(
            parse_args(os(&["--help"])).unwrap().action,
            UsersAction::Help
        );
        assert!(USERS_USAGE.contains("--role admin"));
    }

    #[test]
    fn bootstrap_admin_and_lifecycle() {
        let (dir, cfg) = config_file("life");
        let mut io = Script {
            answers: vec![],
            secret: None,
        };
        // 首位管理員：產生的密碼只在 stdout
        let out = run_cmd(&["create", "admin", "--role", "admin"], &cfg, &mut io);
        assert_eq!(out.exit, 0, "{out:?}");
        let pw = out.stdout.trim().to_string();
        assert_eq!(pw.len(), 23);
        assert!(out.stderr.contains("一次性"));
        // 同名：衝突
        let out = run_cmd(&["create", "ADMIN", "--role", "admin"], &cfg, &mut io);
        assert_eq!(out.exit, EXIT_CONFLICT);
        // --password-stdin：政策
        io.secret = Some("short".into());
        assert_eq!(
            run_cmd(
                &["create", "bob", "--role", "viewer", "--password-stdin"],
                &cfg,
                &mut io
            )
            .exit,
            EXIT_USAGE
        );
        io.secret = Some("correct horse battery".into());
        let out = run_cmd(
            &[
                "create",
                "bob",
                "--role",
                "viewer",
                "--password-stdin",
                "--json",
            ],
            &cfg,
            &mut io,
        );
        assert_eq!(out.exit, 0, "{out:?}");
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["schema"], "kairos.users.create/v1");
        assert!(v["data"].get("initial_password").is_none());
        assert!(!out.stdout.contains("correct horse"));
        // 最後一位管理員：不能停用、刪除、移除角色
        assert_eq!(
            run_cmd(&["disable", "admin", "--yes"], &cfg, &mut io).exit,
            EXIT_REFUSED
        );
        assert_eq!(
            run_cmd(&["delete", "admin", "--yes"], &cfg, &mut io).exit,
            EXIT_REFUSED
        );
        assert_eq!(
            run_cmd(
                &["revoke-role", "admin", "--role", "admin", "--yes"],
                &cfg,
                &mut io
            )
            .exit,
            EXIT_REFUSED
        );
        // 非互動沒有 --yes：用法錯誤
        assert_eq!(run_cmd(&["disable", "bob"], &cfg, &mut io).exit, EXIT_USAGE);
        // 互動確認
        io.answers = vec!["y".into()];
        assert_eq!(run_cmd(&["disable", "bob"], &cfg, &mut io).exit, 0);
        assert_eq!(run_cmd(&["enable", "bob"], &cfg, &mut io).exit, 0);
        // 刪除（L2）：輸入錯的帳號
        io.answers = vec!["alice".into()];
        assert_eq!(run_cmd(&["delete", "bob"], &cfg, &mut io).exit, EXIT_USAGE);
        io.answers = vec!["bob".into()];
        assert_eq!(run_cmd(&["delete", "bob"], &cfg, &mut io).exit, 0);
        assert_eq!(run_cmd(&["show", "bob"], &cfg, &mut io).exit, EXIT_GENERAL);
        // 重設密碼：產生新的，必須變更
        let out = run_cmd(&["reset-password", "admin", "--yes"], &cfg, &mut io);
        assert_eq!(out.exit, 0, "{out:?}");
        assert_ne!(out.stdout.trim(), pw);
        // 群組對應
        assert_eq!(
            run_cmd(
                &[
                    "map-group",
                    "CN=IT-Ops, OU=Groups,DC=city",
                    "--role",
                    "operator"
                ],
                &cfg,
                &mut io
            )
            .exit,
            0
        );
        let out = run_cmd(&["groups", "--json"], &cfg, &mut io);
        assert!(
            out.stdout.contains("cn=it-ops,ou=groups,dc=city"),
            "{}",
            out.stdout
        );
        // 列表與稽核鏈
        let out = run_cmd(&["list", "--all"], &cfg, &mut io);
        assert!(
            out.stdout.contains("admin") && out.stdout.contains("管理員（全站）"),
            "{}",
            out.stdout
        );
        let conn = open(Some(&cfg), false).unwrap().unwrap();
        let r = conn.audit_verify().unwrap();
        assert!(r.ok(), "{r:?}");
        assert!(r.checked >= 6);
        let raw: String = conn
            .test_sqlite()
            .query_row(
                "SELECT group_concat(coalesce(details, ''), ' ') FROM audit_events",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !raw.contains("correct horse") && !raw.contains(&pw),
            "稽核不應含密碼"
        );
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
