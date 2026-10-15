//! `kairos enroll-token`、`kairos enroll`、`kairos pki`、`kairos config`（`kairos/API.md` 第 11.6 節）。
//!
//! 風格依設計包 `cli-style.md`：結果在 stdout、過程與錯誤在 stderr；`--json` 為純 ASCII 的單一文件
//! （`schema` 為 `kairos.<命令>.<動作>/v1`），錯誤也以 JSON 寫到 stdout。危險操作（§5.4）：L1 互動時詢問、
//! 非互動需 `--yes`；L2 另需 `--expect-count`；L3 需 `--confirm <固定字串>`。
//!
//! 以服務帳號執行（`sudo -u kairos`），Kairos 執行中也可以：直接讀寫資料庫，執行中的 Kairos 在 2 秒內
//! 察覺（核准的申請由常駐行程簽發、撤銷後由常駐行程重新產生 CRL、模式立即生效）。CLI 不簽發、不讀
//! 簽發 CA 的私鑰；只有 `kairos pki sign-bundle`（在離線的 CA 主機上執行）讀取指定的私鑰檔。

use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use kairos_proto::enroll::{EnrollMode, TokenApproval};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::config::{AppConfig, SettingSource, take_config_arg};
use crate::enroll::ops::{self, Decision, OpError, Resolved, RevokeResult, TokenInput};
use crate::enroll::store::{self, DbState, RequestFilter};
use crate::hosts_cli::{ascii_json, parse_duration, use_unicode};
use crate::pki::issue::Reason;
use crate::pki::store as ps;
use crate::settings;
use crate::storage::fmt_time;
use crate::token_cli::{
    Failure, Output, db_failure, human_time, human_time_zoned, open, operator, relative, table,
    zone_label,
};

pub(crate) const EXIT_GENERAL: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;
pub(crate) const EXIT_CONFIG: u8 = 3;
pub(crate) const EXIT_CERT: u8 = 4;
const EXIT_REJECTED: u8 = 6;
pub(crate) const EXIT_CONFLICT: u8 = 11;

/// `kairos enroll-token --help`
pub const ENROLL_TOKEN_USAGE: &str = "kairos enroll-token — 註冊 token（S12；crates/kairos-proto/ENROLL.md 第 14.2 節）

用法：
  kairos enroll-token create --name <名稱> --group <群組> --max-uses <N> [--approval manual|auto]
                             [--expires-in 7d | --expires-at <日期>] [--tag <標籤>]... [--allow-cidr <CIDR>]...
                             [--batch] [--allow-agent-id <agent_id>]... [--cert-days <N>]
  kairos enroll-token list [--all | --state active|expired|exhausted|disabled]
  kairos enroll-token show <token_id>
  kairos enroll-token disable <token_id> [--reason <原因>] [--yes]

create 只把 token 寫到 stdout（TOKEN=$(kairos enroll-token create …)）；名稱、CA 指紋與安裝字串寫到
stderr。token 只會顯示這一次，之後只顯示前 4 碼（KRT1-XXXX…）。自動核准需要 --allow-cidr 或 --allow-agent-id。

通用選項：--config <path>、--json、--ascii、-h/--help
結束代碼：0 完成、1 資料庫錯誤、2 用法錯誤、3 設定錯誤、10 權限不足、11 資料庫使用中";

/// `kairos enroll --help`
pub const ENROLL_USAGE: &str = "kairos enroll — 註冊申請與註冊端點模式（S13、S21）

用法：
  kairos enroll list [--closed] [--token <token_id>] [--flag none|any|<旗標>]
  kairos enroll show <request_id | 申請編號>
  kairos enroll approve <request_id | 申請編號>... [--all-clean --token <token_id>] [--cert-days <N>] [--yes]
  kairos enroll reject <request_id | 申請編號>... --reason <原因> [--reason-code <代碼>] [--yes --expect-count <N>]
  kairos enroll mode [open | renew_only | closed] [--auto-pause on|off] [--auto-pause-after-hours <N>]
                     [--reason <原因>] [--yes] [--expect-count <N>]

核准只把申請標為已核准，執行中的 Kairos 在 2 秒內簽發（離線簽發模式改以 kairos pki queue 匯出、匯入）。
mode 不帶模式時只顯示目前狀態與每一項的來源；切換到 closed 且有寬限期內或 30 天內到期的離線主機時為 L2：
非互動需要 --yes --expect-count <N＋M> 與 --reason。

通用選項：--config <path>、--json、--ascii、-h/--help
結束代碼：0 完成、1 資料庫錯誤或批次中有失敗、2 用法錯誤、3 設定錯誤（含被環境變數覆寫）、6 被拒絕、
10 權限不足、11 資料庫使用中";

/// `kairos pki --help`
pub const PKI_USAGE: &str = "kairos pki — 簽發 CA、憑證與 CRL（kairos/PKI.md）

用法：
  # 新安裝：根 CA（P-384）、伺服器憑證與代理簽發 CA 一次建立（以服務帳號執行；只能執行一次）
  kairos pki init --name <DNS 名稱或 IP>... --root-dir <目錄> [--organization <組織>] [--server-days <N>]

  kairos pki gencrl（線上模式：立即重新產生簽發 CA 的 CRL，同主控台 S15「立即產生 CRL」）
  kairos pki revocations export --ca-cert <CA 憑證> [--out <檔案>]（離線模式：匯出撤銷清單）
  kairos pki issuing-ca list
  kairos pki issuing-ca new [--out <csr.pem>]（預設寫到 data/pki/issuing-<ca_id>.csr）
  kairos pki issuing-ca install <cert.pem>
  kairos pki issuing-ca activate <ca_id>
  kairos pki issuing-ca retire <ca_id> --confirm \"重建 CA\" [--compromised]
  kairos pki certificates list [--state <狀態>] [--expires-within <時間>]
  kairos pki revoke <序號 | agent_id>... --reason <原因> --comment <說明> [--yes --expect-count <N>]
  kairos pki queue export [--out <檔案>]
  kairos pki queue import <檔案>

  # 在離線的 CA 主機上（不讀 Kairos 的設定與資料庫）：
  kairos pki sign-bundle --in <檔案> --ca-cert <檔案> --ca-key <檔案> --out <檔案>
  kairos pki sign --profile server --ca-cert <根 CA> --ca-key <根 CA 私鑰> --name <名稱>...
                  (--csr <server.csr> | --new-key <server.key>) --out <server.pem> [--days <N，≤825>]
  kairos pki sign --profile issuing-ca --ca-cert <根 CA> --ca-key <根 CA 私鑰> --csr <issuing-N.csr> --out <檔案>
  kairos pki sign --profile client --ca-cert <CA> --ca-key <私鑰> --agent-id <agent_id> --csr <檔案> --out <檔案>
  kairos pki gencrl --ca-cert <CA> --ca-key <私鑰> [--revocations <檔案>] --out <agents.crl> [--days <N>]

sign、gencrl 在 CA 私鑰所在的目錄附加一筆紀錄到 kairos-ca.log（0600）；私鑰權限過寬（不是 0600）時拒絕。
輸出檔已存在時一律不覆寫。根 CA 可以是 ECDSA P-256／P-384 或既有的 RSA（PKCS#8 PEM）。

簽發 CA 的流程：issuing-ca new 在 data/pki/ 產生私鑰（0600）與 CSR → 在管理主機以根 CA 簽發
（basicConstraints=critical,CA:TRUE,pathlen:0、keyUsage=critical,keyCertSign,cRLSign、extendedKeyUsage=clientAuth）
→ issuing-ca install（第一個簽發 CA 自動啟用）。撤銷的原因：keyCompromise、cessationOfOperation、superseded、unspecified。

通用選項：--config <path>、--json、--ascii、-h/--help
結束代碼：0 完成、1 資料庫錯誤或批次中有失敗、2 用法錯誤、3 設定錯誤、4 憑證錯誤、10 權限不足、11 衝突";

/// `kairos config --help`
pub const CONFIG_USAGE: &str = "kairos config — 設定

用法：
  kairos config show --effective [<設定鍵或前綴>...] [--source env|console|file|default] [--config <path>] [--json]
  kairos config reset <設定鍵>... [--yes] [--reason <原因>] [--expect-count <N>]（改用設定檔的值：刪除主控台的設定）

列出每一個設定鍵實際生效的值與來源：environment（KAIROS__ 環境變數）＞ console（主控台或 CLI 寫入資料庫
的 settings 表）＞ file（kairos.toml）＞ default（程式內建）。reset 會關閉註冊端點或縮短過期寬限期、使主機
無法自動續簽時為 L2：互動輸入「停止續簽 N 台」，非互動需要 --yes --expect-count <N> 與 --reason。
結束代碼：0 完成、2 用法錯誤、3 設定錯誤、6 被拒絕（影響台數已改變）";

/// 命令的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    EnrollToken,
    Enroll,
    Pki,
    Config,
}

/// 解析後的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub group: Group,
    /// 子命令與位置參數
    pub args: Vec<String>,
    /// 選項（可重複）
    pub opts: Vec<(String, String)>,
    pub flags: Vec<String>,
    pub config_path: Option<PathBuf>,
    pub json: bool,
    pub ascii: bool,
    pub help: bool,
}

const FLAGS: [&str; 9] = [
    "--yes",
    "--all",
    "--closed",
    "--batch",
    "--all-clean",
    "--compromised",
    "--effective",
    "--json",
    "--ascii",
];

impl Cmd {
    pub(crate) fn opt(&self, name: &str) -> Option<&str> {
        self.opts
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn opts_all(&self, name: &str) -> Vec<String> {
        self.opts
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub(crate) fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    pub(crate) fn sub(&self, i: usize) -> Option<&str> {
        self.args.get(i).map(String::as_str)
    }

    pub(crate) fn schema(&self) -> String {
        let group = match self.group {
            Group::EnrollToken => "enroll-token",
            Group::Enroll => "enroll",
            Group::Pki => "pki",
            Group::Config => "config",
        };
        let action = match (self.group, self.sub(0), self.sub(1)) {
            (
                Group::Pki,
                Some(a @ ("issuing-ca" | "certificates" | "queue" | "revocations")),
                Some(b),
            ) => {
                format!("{a}.{b}")
            }
            (_, Some(a), _) => a.to_string(),
            _ => "help".to_string(),
        };
        format!("kairos.{group}.{action}/v1")
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

fn usage(group: Group) -> &'static str {
    match group {
        Group::EnrollToken => ENROLL_TOKEN_USAGE,
        Group::Enroll => ENROLL_USAGE,
        Group::Pki => PKI_USAGE,
        Group::Config => CONFIG_USAGE,
    }
}

pub(crate) fn usage_err(msg: impl Into<String>) -> Failure {
    Failure::new(
        "用法錯誤",
        msg,
        vec!["以 --help 查看用法。".into()],
        EXIT_USAGE,
    )
}

fn op_failure(e: OpError) -> Failure {
    let exit = match e.code {
        "invalid_parameter" => EXIT_USAGE,
        "storage_unavailable" => EXIT_GENERAL,
        "token_not_found" | "request_not_found" => EXIT_GENERAL,
        _ => EXIT_REJECTED,
    };
    Failure::new("操作未完成", e.message, Vec::new(), exit)
}

pub(crate) fn load_config(path: Option<&Path>) -> Result<AppConfig, Failure> {
    AppConfig::load(path).map_err(|e| Failure {
        code: Some(crate::diag::config_load_code(&e, path)),
        ..Failure::new(
            "設定檔無法載入",
            e.to_string(),
            vec!["修正設定檔後再執行，或以 --config 指定 Kairos 使用的設定檔。".into()],
            EXIT_CONFIG,
        )
    })
}

pub(crate) fn db(cmd: &Cmd) -> Result<rusqlite::Connection, Failure> {
    match open(cmd.config_path.as_deref(), true)? {
        Some(c) => Ok(c),
        None => Err(db_failure("資料庫不存在")),
    }
}

/// 執行環境（測試可替換）
pub struct Env<'a> {
    pub now: DateTime<Utc>,
    pub tz: FixedOffset,
    pub unicode: bool,
    pub confirm: &'a mut dyn FnMut(&str) -> Option<String>,
}

pub(crate) fn ok_json(cmd: &Cmd, now: DateTime<Utc>, data: Value) -> Output {
    Output {
        stdout: ascii_json(&json!({
            "schema": cmd.schema(), "ok": true, "generated_at": fmt_time(now),
            "data": data, "warnings": [], "error": null,
        })) + "\n",
        ..Default::default()
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
        Group::EnrollToken => token_cmd(cmd, env),
        Group::Enroll => enroll_cmd(cmd, env),
        Group::Pki => pki_cmd(cmd, env),
        Group::Config => config_cmd(cmd, env),
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

pub(crate) fn mark(env: &Env<'_>) -> &'static str {
    if env.unicode { "✓" } else { "[ OK ]" }
}

fn parse_expiry(cmd: &Cmd, now_ms: i64) -> Result<Option<i64>, Failure> {
    match (cmd.opt("--expires-in"), cmd.opt("--expires-at")) {
        (Some(_), Some(_)) => Err(usage_err("--expires-in、--expires-at 只能擇一")),
        (Some(v), None) => parse_duration(v)
            .filter(|d| !d.is_zero())
            .map(|d| Some(now_ms + d.as_millis() as i64))
            .ok_or_else(|| usage_err(format!("--expires-in 的值 \"{v}\" 無效，例如 7d、12h"))),
        (None, Some(v)) => {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d") {
                return Ok(Some(
                    d.and_hms_opt(0, 0, 0)
                        .unwrap_or_default()
                        .and_utc()
                        .timestamp_millis(),
                ));
            }
            DateTime::parse_from_rfc3339(v)
                .map(|t| Some(t.timestamp_millis()))
                .map_err(|_| {
                    usage_err(format!(
                        "--expires-at 的值 \"{v}\" 無效（2027-01-01 或 RFC 3339）"
                    ))
                })
        }
        (None, None) => Ok(None),
    }
}

pub(crate) fn int_opt(cmd: &Cmd, name: &str) -> Result<Option<i64>, Failure> {
    cmd.opt(name)
        .map(|v| {
            v.parse::<i64>()
                .map_err(|_| usage_err(format!("{name} 必須是整數")))
        })
        .transpose()
}

/// 安裝字串與 CA 指紋（以設定推導；註冊端點未監聽時也可以產生）
fn install_info(
    config: &AppConfig,
    token: &str,
    tags: Option<Vec<String>>,
) -> (Option<String>, Option<String>) {
    let Some(path) = config.pki.server_ca() else {
        return (None, None);
    };
    let Ok(ca) = crate::enroll::pki_mgr::load_server_ca(path) else {
        return (None, None);
    };
    let adv = config.enroll.advertise.trim();
    let server = config.server.listen.trim();
    let agent = match adv.rsplit_once(':') {
        Some((h, _)) if !adv.is_empty() => {
            let port = server.rsplit_once(':').map_or("8443", |(_, p)| p);
            format!("{h}:{port}")
        }
        _ => server.to_string(),
    };
    let s = kairos_proto::enroll::normalize_endpoint(&agent).ok();
    // 註冊端點：advertise，沒有時為 enroll.listen（與主控台 API 相同）；等於「s 的主機:8444」時省略
    let enroll_endpoint = if adv.is_empty() {
        config.enroll.listen_addr(server)
    } else {
        adv.to_string()
    };
    let derived = s.as_ref().and_then(|s| {
        kairos_proto::enroll::normalize_endpoint(&format!(
            "{}:{}",
            kairos_proto::enroll::endpoint_host(s),
            kairos_proto::enroll::DEFAULT_ENROLL_PORT
        ))
        .ok()
    });
    let e = kairos_proto::enroll::normalize_endpoint(&enroll_endpoint)
        .ok()
        .filter(|e| Some(e) != derived.as_ref());
    let install = s.and_then(|s| {
        kairos_proto::enroll::InstallString {
            s,
            e,
            t: token.to_string(),
            p: ca.pin.clone(),
            g: tags,
        }
        .encode()
        .ok()
    });
    (install, Some(ca.pin))
}

// ---------------------------------------------------------------------------
// enroll-token
// ---------------------------------------------------------------------------

fn token_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    match cmd.sub(0) {
        Some("create") => {
            let approval = match cmd.opt("--approval").unwrap_or("manual") {
                "manual" => TokenApproval::Manual,
                "auto" => TokenApproval::Auto,
                v => {
                    return Err(usage_err(format!(
                        "--approval 的值 \"{v}\" 無效（manual、auto）"
                    )));
                }
            };
            let tags = cmd.opts_all("--tag");
            let cidrs = cmd.opts_all("--allow-cidr");
            let ids = cmd.opts_all("--allow-agent-id");
            let input = TokenInput {
                name: cmd.opt("--name").unwrap_or("").to_string(),
                group: cmd.opt("--group").unwrap_or("").to_string(),
                tags: (!tags.is_empty()).then_some(tags),
                expires_at: parse_expiry(cmd, now_ms)?,
                max_uses: int_opt(cmd, "--max-uses")?
                    .ok_or_else(|| usage_err("--max-uses 必填"))?,
                approval: Some(approval),
                allowed_cidrs: (!cidrs.is_empty()).then_some(cidrs),
                batch_mode: cmd.flag("--batch"),
                allowed_agent_ids: (!ids.is_empty()).then_some(ids),
                cert_lifetime_days: int_opt(cmd, "--cert-days")?,
            };
            let config = load_config(cmd.config_path.as_deref())?;
            let mut conn = db(cmd)?;
            let created = ops::create_token(&mut conn, &input, &operator(), None, now_ms)
                .map_err(op_failure)?;
            let token = created.token.canonical();
            let (install, pin) = install_info(&config, &token, created.row.tags.clone());
            if cmd.json {
                let mut data = ops::token_json(&created.row, now_ms);
                data["token"] = json!(token);
                data["install_string"] = json!(install);
                data["ca_pin"] = json!(pin);
                return Ok(ok_json(cmd, env.now, data));
            }
            let mut err = format!(
                "{} 已建立註冊 token「{}」（token_id {}，{}核准，最多 {} 次，到期 {}）\n",
                mark(env),
                created.row.name,
                created.row.token_id,
                if approval == TokenApproval::Auto {
                    "自動"
                } else {
                    "需"
                },
                created.row.max_uses,
                human_time_zoned(created.row.expires_at, env.tz)
            );
            if let Some(pin) = &pin
                && let Ok([a, b]) = kairos_proto::enroll::ca_pin_display(pin)
            {
                err.push_str(&format!("  CA 指紋：{a}\n          {b}\n"));
            }
            match &install {
                Some(s) => err.push_str(&format!("  安裝字串：{s}\n")),
                None => err.push_str("  （未設定 pki.server_ca_path，無法產生安裝字串）\n"),
            }
            err.push_str("  token 只會顯示這一次，請以安全管道傳遞；之後只顯示前 4 碼。\n");
            Ok(Output {
                stdout: format!("{token}\n"),
                stderr: err,
                exit: 0,
            })
        }
        Some("list") => {
            let state = if cmd.flag("--all") {
                "all".to_string()
            } else {
                cmd.opt("--state").unwrap_or("active").to_string()
            };
            let conn = db(cmd)?;
            let rows = store::list_tokens(&conn).map_err(db_failure)?;
            let shown: Vec<_> = rows
                .iter()
                .filter(|t| state == "all" || t.state(now_ms).as_str() == state)
                .collect();
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!({"tokens": shown.iter().map(|t| ops::token_json(t, now_ms)).collect::<Vec<_>>()}),
                ));
            }
            if shown.is_empty() {
                return Ok(Output {
                    stdout: "沒有符合的註冊 token。\n".into(),
                    ..Default::default()
                });
            }
            let zone = zone_label(env.tz);
            let mut t = vec![vec![
                "TOKEN_ID".to_string(),
                "TOKEN".into(),
                "名稱".into(),
                "群組".into(),
                "核准".into(),
                "已用/上限".into(),
                format!("有效至（{zone}）"),
                "狀態".into(),
            ]];
            for r in &shown {
                t.push(vec![
                    r.token_id.clone(),
                    r.hint.clone(),
                    r.name.clone(),
                    r.group_name.clone(),
                    store::approval_str(r.approval).into(),
                    format!("{}/{}", r.used, r.max_uses),
                    human_time(r.expires_at, env.tz),
                    token_state_label(r.state(now_ms)).into(),
                ]);
            }
            Ok(Output {
                stdout: format!("{}\n共 {} 個\n", table(&t), shown.len()),
                ..Default::default()
            })
        }
        Some("show") => {
            let id = cmd.sub(1).ok_or_else(|| usage_err("需要 token_id"))?;
            let conn = db(cmd)?;
            let t = store::get_token(&conn, id)
                .map_err(db_failure)?
                .ok_or_else(|| {
                    Failure::new(
                        format!("找不到註冊 token {id}"),
                        "資料庫中沒有這個 token_id",
                        Vec::new(),
                        EXIT_GENERAL,
                    )
                })?;
            let mut v = ops::token_json(&t, now_ms);
            v["progress"] = ops::token_progress(&conn, id).map_err(op_failure)?;
            if cmd.json {
                return Ok(ok_json(cmd, env.now, v));
            }
            let p = &v["progress"];
            Ok(Output {
                stdout: format!(
                    "{}（{}）\n  名稱 {}，群組 {}，{}核准，已用 {}/{}，有效至 {}，狀態 {}\n  部署進度：已送出 {}、待核准 {}、已簽發 {}（已上線 {}）、已拒絕 {}、已撤回 {}\n",
                    t.hint,
                    t.token_id,
                    t.name,
                    t.group_name,
                    if t.approval == TokenApproval::Auto {
                        "自動"
                    } else {
                        "需"
                    },
                    t.used,
                    t.max_uses,
                    human_time_zoned(t.expires_at, env.tz),
                    token_state_label(t.state(now_ms)),
                    p["used"],
                    p["pending"],
                    p["issued"],
                    p["online"],
                    p["rejected"],
                    p["withdrawn"]
                ),
                ..Default::default()
            })
        }
        Some("disable") => {
            let id = cmd.sub(1).ok_or_else(|| usage_err("需要 token_id"))?;
            if !cmd.flag("--yes") {
                let ok = (env.confirm)(&format!("即將停用註冊 token {id}：之後以它送出的註冊一律被拒（已簽發的憑證不受影響）。\n是否繼續？[y/N] "))
                    .is_some_and(|a| matches!(a.trim(), "y" | "Y" | "yes"));
                if !ok {
                    return Err(Failure::new(
                        "未停用",
                        "沒有確認（非互動執行時需要 --yes）",
                        Vec::new(),
                        EXIT_USAGE,
                    ));
                }
            }
            let mut conn = db(cmd)?;
            let (row, already) = ops::disable_token(
                &mut conn,
                id,
                &operator(),
                None,
                cmd.opt("--reason"),
                now_ms,
            )
            .map_err(op_failure)?;
            if cmd.json {
                let mut v = ops::token_json(&row, now_ms);
                v["already_disabled"] = json!(already);
                return Ok(ok_json(cmd, env.now, v));
            }
            Ok(Output {
                stdout: format!(
                    "{} 註冊 token {}（{}）{}\n",
                    mark(env),
                    row.token_id,
                    row.name,
                    if already {
                        "先前已停用"
                    } else {
                        "已停用"
                    }
                ),
                ..Default::default()
            })
        }
        Some(s) => Err(usage_err(format!(
            "無法辨識的子命令: {s}（create、list、show、disable）"
        ))),
        None => Err(usage_err("需要子命令")),
    }
}

fn token_state_label(s: store::TokenState) -> &'static str {
    match s {
        store::TokenState::Active => "有效",
        store::TokenState::Expired => "已過期",
        store::TokenState::Exhausted => "已用完",
        store::TokenState::Disabled => "已停用",
    }
}

// ---------------------------------------------------------------------------
// enroll
// ---------------------------------------------------------------------------

fn resolve_all(conn: &rusqlite::Connection, inputs: &[String]) -> Result<Vec<String>, Failure> {
    let mut out = Vec::new();
    for i in inputs {
        match ops::resolve_request(conn, i).map_err(op_failure)? {
            Resolved::One(r) => out.push(r.request_id),
            Resolved::NotFound => {
                return Err(Failure::new(
                    format!("找不到申請 {i}"),
                    "沒有這個 request_id 或申請編號",
                    Vec::new(),
                    EXIT_GENERAL,
                ));
            }
            Resolved::Ambiguous(rows) => {
                return Err(Failure::new(
                    format!("申請編號 {i} 對到多筆申請"),
                    format!(
                        "候選：{}",
                        rows.iter()
                            .map(|r| r.request_id.clone())
                            .collect::<Vec<_>>()
                            .join("、")
                    ),
                    vec!["改用 request_id。".into()],
                    EXIT_USAGE,
                ));
            }
        }
    }
    Ok(out)
}

fn enroll_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    match cmd.sub(0) {
        Some("list") => {
            let conn = db(cmd)?;
            let filter = RequestFilter {
                closed: cmd.flag("--closed"),
                token_id: cmd.opt("--token").map(str::to_string),
                kind: None,
                state: None,
                limit: 100_000,
            };
            let flag = cmd.opt("--flag");
            let rows: Vec<_> = store::list_requests(&conn, &filter)
                .map_err(db_failure)?
                .into_iter()
                .filter(|r| match flag {
                    None => true,
                    Some("none") => r.flags.is_empty(),
                    Some("any") => !r.flags.is_empty(),
                    Some(f) => r.flags.iter().any(|x| x == f),
                })
                .collect();
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!({"requests": rows.iter().map(ops::request_json).collect::<Vec<_>>()}),
                ));
            }
            if rows.is_empty() {
                return Ok(Output {
                    stdout: "沒有符合的申請。\n".into(),
                    ..Default::default()
                });
            }
            let mut t = vec![vec![
                "申請編號".to_string(),
                "主機名稱".into(),
                "AGENT_ID".into(),
                "來源 IP".into(),
                "種類".into(),
                "狀態".into(),
                "TOKEN".into(),
                "送出".into(),
                "異常旗標".into(),
            ]];
            for r in &rows {
                t.push(vec![
                    ops::request_json(r)["request_code"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    r.hostname
                        .chars()
                        .map(|c| if c.is_control() { '?' } else { c })
                        .collect(),
                    r.agent_id[..8].to_string(),
                    r.source_ip.clone(),
                    r.kind.as_str().into(),
                    r.state.as_str().into(),
                    r.token_id.clone().unwrap_or_else(|| "-".into()),
                    relative(now_ms, r.created_at),
                    if r.flags.is_empty() {
                        "-".into()
                    } else {
                        r.flags.join(",")
                    },
                ]);
            }
            Ok(Output {
                stdout: format!("{}\n共 {} 筆\n", table(&t), rows.len()),
                ..Default::default()
            })
        }
        Some("show") => {
            let input = cmd
                .sub(1)
                .ok_or_else(|| usage_err("需要 request_id 或申請編號"))?;
            let conn = db(cmd)?;
            let id = resolve_all(&conn, &[input.to_string()])?.remove(0);
            let r = store::get_request(&conn, &id)
                .map_err(db_failure)?
                .ok_or_else(|| db_failure("申請已不存在"))?;
            let v = ops::request_json(&r);
            if cmd.json {
                return Ok(ok_json(cmd, env.now, v));
            }
            Ok(Output {
                stdout: format!(
                    "申請 {}（request_id {}）\n  種類 {}，狀態 {}\n  主機 {}（agent_id {}），來源 {}\n  公鑰指紋前 8 碼 {}\n  送出 {}，有效至 {}\n  異常旗標 {}\n",
                    v["request_code"].as_str().unwrap_or(""),
                    r.request_id,
                    r.kind.as_str(),
                    r.state.as_str(),
                    r.hostname,
                    r.agent_id,
                    r.source_ip,
                    v["spki_fingerprint_short"].as_str().unwrap_or(""),
                    human_time_zoned(r.created_at, env.tz),
                    human_time_zoned(r.expires_at, env.tz),
                    if r.flags.is_empty() {
                        "-".into()
                    } else {
                        r.flags.join("、")
                    }
                ),
                ..Default::default()
            })
        }
        Some("approve") => {
            let mut conn = db(cmd)?;
            let mut ids = resolve_all(&conn, &cmd.args[1..])?;
            if cmd.flag("--all-clean") {
                let token = cmd
                    .opt("--token")
                    .ok_or_else(|| usage_err("--all-clean 需要 --token <token_id>"))?;
                let filter = RequestFilter {
                    closed: false,
                    token_id: Some(token.to_string()),
                    kind: None,
                    state: Some(DbState::PendingApproval),
                    limit: 100_000,
                };
                ids.extend(
                    store::list_requests(&conn, &filter)
                        .map_err(db_failure)?
                        .into_iter()
                        .filter(|r| r.flags.is_empty())
                        .map(|r| r.request_id),
                );
            }
            if ids.is_empty() {
                return Err(usage_err("需要至少一筆申請"));
            }
            if !cmd.flag("--yes") {
                let list = if ids.len() <= 3 {
                    format!("（{}）", ids.join("、"))
                } else {
                    String::new()
                };
                let ok = (env.confirm)(&format!(
                    "即將核准 {} 筆申請{list}。\n是否繼續？[y/N] ",
                    ids.len()
                ))
                .is_some_and(|a| matches!(a.trim(), "y" | "Y" | "yes"));
                if !ok {
                    return Err(Failure::new(
                        "未核准",
                        "沒有確認（非互動執行時需要 --yes）",
                        Vec::new(),
                        EXIT_USAGE,
                    ));
                }
            }
            let lifetime = int_opt(cmd, "--cert-days")?;
            let results = ops::approve(&mut conn, &ids, &operator(), None, lifetime, now_ms)
                .map_err(op_failure)?;
            decisions_output(cmd, env, "核准", results)
        }
        Some("reject") => {
            let reason = cmd
                .opt("--reason")
                .ok_or_else(|| usage_err("--reason 必填"))?;
            let mut conn = db(cmd)?;
            let ids = resolve_all(&conn, &cmd.args[1..])?;
            if ids.is_empty() {
                return Err(usage_err("需要至少一筆申請"));
            }
            if cmd.flag("--yes") {
                let expect = int_opt(cmd, "--expect-count")?.ok_or_else(|| {
                    usage_err("非互動執行時需要 --yes 與 --expect-count <N>（L2）")
                })?;
                if expect as usize != ids.len() {
                    return Err(Failure::new(
                        "未拒絕",
                        format!("--expect-count {expect} 與實際筆數 {} 不同", ids.len()),
                        Vec::new(),
                        EXIT_USAGE,
                    ));
                }
            } else {
                let want = format!("拒絕 {} 筆", ids.len());
                let typed = (env.confirm)(&format!(
                    "即將拒絕 {} 筆申請，原因「{reason}」會顯示在主機端。\n請輸入「{want}」確認：",
                    ids.len()
                ));
                if typed.as_deref().map(str::trim) != Some(want.as_str()) {
                    return Err(Failure::new(
                        "未拒絕",
                        "確認字串不符（非互動執行時需要 --yes 與 --expect-count）",
                        Vec::new(),
                        EXIT_USAGE,
                    ));
                }
            }
            let code = cmd.opt("--reason-code").unwrap_or("other");
            let results = ops::reject(&mut conn, &ids, &operator(), None, code, reason, now_ms)
                .map_err(op_failure)?;
            decisions_output(cmd, env, "拒絕", results)
        }
        Some("mode") => mode_cmd(cmd, env),
        Some(s) => Err(usage_err(format!(
            "無法辨識的子命令: {s}（list、show、approve、reject、mode）"
        ))),
        None => Err(usage_err("需要子命令")),
    }
}

fn decisions_output(
    cmd: &Cmd,
    env: &Env<'_>,
    verb: &str,
    results: Vec<(String, Decision)>,
) -> Result<Output, Failure> {
    let failed = results.iter().any(|(_, d)| matches!(d, Decision::NotFound));
    if cmd.json {
        let mut out = ok_json(
            cmd,
            env.now,
            json!({"results": results.iter().map(|(id, d)| {
            let mut v = json!({"request_id": id, "result": d.as_str()});
            if let Decision::AlreadyDecided(s) = d { v["state"] = json!(s.as_str()); }
            v
        }).collect::<Vec<_>>()}),
        );
        out.exit = if failed { EXIT_GENERAL } else { 0 };
        return Ok(out);
    }
    let mut text = String::new();
    for (id, d) in &results {
        let what = match d {
            Decision::Approved => "已核准（執行中的 Kairos 在 2 秒內簽發；離線簽發模式以 kairos pki queue export 匯出）".to_string(),
            Decision::Rejected => "已拒絕".into(),
            Decision::AlreadyDecided(s) => format!("先前已決定（{}）", s.as_str()),
            Decision::Withdrawn => "申請者已撤回（不算失敗）".into(),
            Decision::NotFound => "找不到".into(),
        };
        text.push_str(&format!("{} {verb} {id}：{what}\n", mark(env)));
    }
    Ok(Output {
        stdout: text,
        exit: if failed { EXIT_GENERAL } else { 0 },
        ..Default::default()
    })
}

fn mode_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    let config = load_config(cmd.config_path.as_deref())?;
    let mut conn = db(cmd)?;
    let overrides = settings::overrides_from(&store::load_settings(&conn).map_err(db_failure)?);
    let eff = settings::effective(&config, &overrides);
    let mode =
        match cmd.sub(1) {
            None => None,
            Some(m) => Some(EnrollMode::parse(m).ok_or_else(|| {
                usage_err(format!("模式 \"{m}\" 無效（open、renew_only、closed）"))
            })?),
        };
    let auto = match cmd.opt("--auto-pause") {
        None => None,
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some(v) => {
            return Err(usage_err(format!(
                "--auto-pause 的值 \"{v}\" 無效（on、off）"
            )));
        }
    };
    let hours = int_opt(cmd, "--auto-pause-after-hours")?.map(|h| h as u32);
    let show = |eff: &settings::Effective| -> Value {
        json!({
            "mode": eff.mode.as_str(),
            "auto_pause_enabled": eff.auto_pause_enabled,
            "auto_pause_after_hours": eff.auto_pause_after_hours,
            "sources": eff.sources_json(),
        })
    };
    if mode.is_none() && auto.is_none() && hours.is_none() {
        if cmd.json {
            return Ok(ok_json(cmd, env.now, show(&eff)));
        }
        return Ok(Output {
            stdout: format!(
                "註冊端點模式：{}（來源 {}）\n自動暫停：{}，{} 小時沒有有效 token 後暫停（來源 {}／{}）\n",
                eff.mode.as_str(),
                eff.source(settings::KEY_MODE).as_str(),
                if eff.auto_pause_enabled {
                    "啟用"
                } else {
                    "停用"
                },
                eff.auto_pause_after_hours,
                eff.source(settings::KEY_AUTO_PAUSE_ENABLED).as_str(),
                eff.source(settings::KEY_AUTO_PAUSE_AFTER_HOURS).as_str()
            ),
            ..Default::default()
        });
    }
    for (set, key) in [
        (mode.is_some(), settings::KEY_MODE),
        (auto.is_some(), settings::KEY_AUTO_PAUSE_ENABLED),
        (hours.is_some(), settings::KEY_AUTO_PAUSE_AFTER_HOURS),
    ] {
        if set && config.provenance.source(key) == SettingSource::Environment {
            return Err(Failure::new(
                format!("{key} 由環境變數設定，無法修改"),
                format!(
                    "環境變數 {} 優先於主控台與 CLI",
                    crate::config::Provenance::env_var(key)
                ),
                vec![format!(
                    "移除 {} 後重新啟動 Kairos，再執行這個命令。",
                    crate::config::Provenance::env_var(key)
                )],
                EXIT_CONFIG,
            ));
        }
    }
    let impact =
        ops::impact(&conn, eff.policy.expired_renew_grace_days, now_ms).map_err(op_failure)?;
    let target = mode.unwrap_or(eff.mode);
    let (level, confirm_text) = impact.level(target);
    if level == "L2" {
        let n = impact.n + impact.m;
        if cmd.opt("--reason").is_none() {
            return Err(usage_err(format!(
                "切換到 closed 會讓 {n} 台主機無法自動續簽（L2）：--reason 必填"
            )));
        }
        if cmd.flag("--yes") {
            let expect = int_opt(cmd, "--expect-count")?
                .ok_or_else(|| usage_err("非互動執行時需要 --yes --expect-count <N＋M>（L2）"))?;
            if expect != n {
                return Err(Failure::new(
                    "未變更",
                    format!("影響台數已改變：目前 N＋M＝{n}（--expect-count {expect}）"),
                    vec!["確認數字後以新的 --expect-count 重新執行。".into()],
                    EXIT_REJECTED,
                ));
            }
        } else {
            let want = confirm_text.unwrap_or_default();
            let typed = (env.confirm)(&format!(
                "切換到 closed：寬限期內 {} 台、30 天內進入寬限期且離線 {} 台將無法自動續簽；待核准 {} 筆、有效 token {} 個。\n請輸入「{want}」確認：",
                impact.n, impact.m, impact.r, impact.t
            ));
            if typed.as_deref().map(str::trim) != Some(want.as_str()) {
                return Err(Failure::new(
                    "未變更",
                    "確認字串不符（非互動執行時需要 --yes --expect-count）",
                    Vec::new(),
                    EXIT_USAGE,
                ));
            }
        }
    } else if mode.is_some() && !cmd.flag("--yes") {
        let ok = (env.confirm)(&format!(
            "將註冊端點模式改為 {}。\n是否繼續？[y/N] ",
            target.as_str()
        ))
        .is_some_and(|a| matches!(a.trim(), "y" | "Y" | "yes"));
        if !ok {
            return Err(Failure::new(
                "未變更",
                "沒有確認（非互動執行時需要 --yes）",
                Vec::new(),
                EXIT_USAGE,
            ));
        }
    }
    ops::set_mode(
        &mut conn,
        mode,
        auto,
        hours,
        &operator(),
        None,
        cmd.opt("--reason"),
        &impact,
        level,
        now_ms,
    )
    .map_err(op_failure)?;
    let overrides = settings::overrides_from(&store::load_settings(&conn).map_err(db_failure)?);
    let eff = settings::effective(&config, &overrides);
    if cmd.json {
        return Ok(ok_json(cmd, env.now, show(&eff)));
    }
    Ok(Output {
        stdout: format!(
            "{} 註冊端點模式：{}（執行中的 Kairos 在 2 秒內生效）\n",
            mark(env),
            eff.mode.as_str()
        ),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// pki
// ---------------------------------------------------------------------------

pub(crate) fn write_new_file(path: &str, content: &[u8]) -> Result<(), Failure> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            let exit = if e.kind() == ErrorKind::AlreadyExists {
                EXIT_CONFLICT
            } else {
                EXIT_GENERAL
            };
            Failure::new(
                format!("無法寫入 {path}"),
                e.to_string(),
                vec!["輸出檔已存在時不覆寫；換一個檔名。".into()],
                exit,
            )
        })?;
    f.write_all(content).map_err(|e| {
        Failure::new(
            format!("無法寫入 {path}"),
            e.to_string(),
            Vec::new(),
            EXIT_GENERAL,
        )
    })
}

pub(crate) fn read_file(path: &str) -> Result<String, Failure> {
    std::fs::read_to_string(path).map_err(|e| {
        Failure::new(
            format!("無法讀取 {path}"),
            e.to_string(),
            Vec::new(),
            EXIT_GENERAL,
        )
    })
}

/// 匯入的憑證（簽發 CA、離線簽發的用戶端憑證）未通過檢查：`KRS-E1305`
fn cert_fail(msg: impl Into<String>) -> Failure {
    Failure {
        code: Some("KRS-E1305"),
        ..Failure::new("匯入的憑證未通過檢查", msg, Vec::new(), EXIT_CERT)
    }
}

fn pki_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    match (cmd.sub(0), cmd.sub(1)) {
        (Some("sign-bundle"), _) => sign_bundle(cmd, env),
        (Some("init"), _) => crate::pki_cli::init(cmd, env),
        (Some("sign"), _) => crate::pki_cli::sign(cmd, env),
        (Some("gencrl"), _) => crate::pki_cli::gencrl(cmd, env),
        (Some("revocations"), Some("export")) => crate::pki_cli::revocations_export(cmd, env),
        (Some("issuing-ca"), Some("list")) => {
            let conn = db(cmd)?;
            let cas = ps::list_cas(&conn).map_err(db_failure)?;
            let data: Vec<Value> = cas.iter().map(|c| json!({
                "ca_id": c.ca_id, "subject": c.subject, "state": c.state,
                "not_after": c.not_after.map(crate::api::http::ts_ms), "key_file": c.key_file,
            })).collect();
            if cmd.json {
                return Ok(ok_json(cmd, env.now, json!({"issuing_cas": data})));
            }
            let mut t = vec![vec![
                "CA_ID".to_string(),
                "狀態".into(),
                "SUBJECT".into(),
                format!("到期（{}）", zone_label(env.tz)),
            ]];
            for c in &cas {
                t.push(vec![
                    c.ca_id.to_string(),
                    c.state.clone(),
                    c.subject.clone(),
                    c.not_after
                        .map(|t| human_time(t, env.tz))
                        .unwrap_or_else(|| "-".into()),
                ]);
            }
            Ok(Output {
                stdout: table(&t),
                ..Default::default()
            })
        }
        (Some("issuing-ca"), Some("new")) => {
            let config = load_config(cmd.config_path.as_deref())?;
            let conn = db(cmd)?;
            let data_dir = PathBuf::from(&config.storage.data_dir);
            let (id, n) = crate::pki::manage::create(
                &conn,
                &data_dir,
                config.pki.organization.trim(),
                now_ms,
            )
            .map_err(|e| Failure::new("無法建立簽發 CA", e, Vec::new(), EXIT_GENERAL))?;
            let key_path = data_dir.join(crate::pki::key_file_name(id));
            // CSR：預設寫在私鑰旁（data/pki/issuing-<ca_id>.csr），--out 可另外指定
            let csr_path = cmd.opt("--out").map(PathBuf::from).unwrap_or_else(|| {
                crate::pki::pki_dir(&data_dir).join(format!("issuing-{id}.csr"))
            });
            write_new_file(&csr_path.to_string_lossy(), n.csr_pem.as_bytes())?;
            let _ = store::audit(
                &conn,
                now_ms,
                &store::Audit {
                    actor: &operator(),
                    category: "pki",
                    action: "ca.new",
                    object: Some(&id.to_string()),
                    result: "ok",
                    ..Default::default()
                },
            );
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!({"ca_id": id, "subject": n.subject, "state": "pending", "csr_pem": n.csr_pem, "csr_path": csr_path, "key_path": key_path}),
                ));
            }
            let m = mark(env);
            Ok(Output {
                stdout: format!(
                    "[1/2] {m} 產生簽發 CA 的私鑰（ECDSA P-256）：{}（0600）\n[2/2] {m} 產生憑證簽署要求（CSR）：{}\n\n{m} 已建立簽發 CA #{id}（狀態：等待根 CA 簽發）。私鑰不會離開這台伺服器。\n  下一步：\n    1. 把 issuing-{id}.csr 帶到管理主機，以根 CA 簽發（範本 issuing_ca_ext：CA:TRUE、pathlen:0、\n       keyCertSign／cRLSign、EKU clientAuth，有效 5 年；INSTALL 第 6 節）\n    2. sudo -u kairos /opt/kairos/kairos pki issuing-ca install issuing-{id}.pem\n    3. sudo -u kairos /opt/kairos/kairos pki issuing-ca activate {id}（第一個簽發 CA 安裝時自動啟用）\n",
                    key_path.display(),
                    csr_path.display()
                ),
                ..Default::default()
            })
        }
        (Some("issuing-ca"), Some("install")) => {
            let path = cmd.sub(2).ok_or_else(|| usage_err("需要 cert.pem"))?;
            let config = load_config(cmd.config_path.as_deref())?;
            let pem = read_file(path)?;
            let conn = db(cmd)?;
            let data_dir = PathBuf::from(&config.storage.data_dir);
            let r =
                crate::pki::manage::install(&conn, &data_dir, &pem, now_ms).map_err(cert_fail)?;
            let _ = store::audit(
                &conn,
                now_ms,
                &store::Audit {
                    actor: &operator(),
                    category: "pki",
                    action: "ca.install",
                    object: Some(&r.ca_id.to_string()),
                    result: "ok",
                    details: Some(json!({"activated": r.activated})),
                    ..Default::default()
                },
            );
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!({"ca_id": r.ca_id, "subject": r.subject, "issuer": r.issuer, "activated": r.activated, "state": if r.activated { "active" } else { "trusted" }, "not_after": crate::api::http::ts_ms(r.not_after_ms)}),
                ));
            }
            let m = mark(env);
            let years = (r.not_after_ms - now_ms) as f64 / (365.0 * 86_400_000.0);
            Ok(Output {
                stdout: format!(
                    "{m} 憑證與私鑰成對\n{m} 擴充欄位：CA:TRUE、pathlen:0、keyCertSign／cRLSign、EKU clientAuth\n{m} 簽發者：{}\n{m} 有效期間：至 {}（剩 {years:.1} 年，至少需 2 年）\n{m} 已安裝簽發 CA #{}（{}）\n",
                    r.issuer,
                    human_time(r.not_after_ms, env.tz)
                        .split(' ')
                        .next()
                        .unwrap_or_default(),
                    r.ca_id,
                    if r.activated {
                        "狀態：啟用中。這是第一個簽發 CA，已自動啟用；執行中的 Kairos 在 2 秒內開始線上簽發".to_string()
                    } else {
                        format!(
                            "狀態：受信任，尚未啟用。執行 kairos pki issuing-ca activate {} 後新簽發改用它",
                            r.ca_id
                        )
                    }
                ),
                ..Default::default()
            })
        }
        (Some("issuing-ca"), Some(action @ ("activate" | "retire"))) => {
            let id: i64 = cmd
                .sub(2)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| usage_err("需要 ca_id"))?;
            let conn = db(cmd)?;
            let ca = ps::get_ca(&conn, id).map_err(db_failure)?.ok_or_else(|| {
                Failure::new(format!("找不到簽發 CA {id}"), "", Vec::new(), EXIT_GENERAL)
            })?;
            if action == "activate" {
                if ca.state != ps::CA_TRUSTED && ca.state != ps::CA_ACTIVE {
                    return Err(Failure::new(
                        format!("簽發 CA {id} 的狀態是 {}，不能啟用", ca.state),
                        "只有已安裝（trusted）的簽發 CA 可以啟用",
                        Vec::new(),
                        EXIT_REJECTED,
                    ));
                }
                ps::activate_ca(&conn, id).map_err(db_failure)?;
            } else {
                if cmd.opt("--confirm") != Some("重建 CA") {
                    return Err(usage_err(
                        "退役簽發 CA 是 L3 操作：需要 --confirm \"重建 CA\"",
                    ));
                }
                if ca.state == ps::CA_ACTIVE && !cmd.flag("--compromised") {
                    return Err(Failure::new(
                        format!("簽發 CA {id} 仍在使用中"),
                        "先啟用新的簽發 CA，等主機輪替後再退役（私鑰外洩時加 --compromised）",
                        Vec::new(),
                        EXIT_REJECTED,
                    ));
                }
                ps::retire_ca(&conn, id, now_ms).map_err(db_failure)?;
            }
            let _ = store::audit(
                &conn,
                now_ms,
                &store::Audit {
                    actor: &operator(),
                    category: "pki",
                    action: if action == "activate" {
                        "ca.activate"
                    } else {
                        "ca.retire"
                    },
                    object: Some(&id.to_string()),
                    result: "ok",
                    details: Some(json!({"compromised": cmd.flag("--compromised")})),
                    ..Default::default()
                },
            );
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!({"ca_id": id, "action": action}),
                ));
            }
            Ok(Output {
                stdout: format!(
                    "{} 簽發 CA {id} 已{}\n",
                    mark(env),
                    if action == "activate" {
                        "啟用"
                    } else {
                        "退役（從信任錨移除）"
                    }
                ),
                ..Default::default()
            })
        }
        (Some("certificates"), Some("list")) => {
            let config = load_config(cmd.config_path.as_deref())?;
            let conn = db(cmd)?;
            let overrides =
                settings::overrides_from(&store::load_settings(&conn).map_err(db_failure)?);
            let grace = settings::effective(&config, &overrides)
                .policy
                .expired_renew_grace_days;
            let rows = ps::list_certs(&conn, 1_000_000, None).map_err(db_failure)?;
            let within = cmd
                .opt("--expires-within")
                .map(|v| {
                    parse_duration(v)
                        .map(|d| d.as_millis() as i64)
                        .ok_or_else(|| usage_err("--expires-within 例如 30d"))
                })
                .transpose()?;
            let shown: Vec<_> = rows
                .iter()
                .filter(|c| {
                    cmd.opt("--state")
                        .is_none_or(|s| cert_state(c, grace, now_ms) == s)
                })
                .filter(|c| {
                    within.is_none_or(|w| c.not_after > now_ms && c.not_after - now_ms <= w)
                })
                .collect();
            if cmd.json {
                return Ok(ok_json(
                    cmd,
                    env.now,
                    json!({"certificates": shown.iter().map(|c| json!({
                    "serial": c.serial, "agent_id": c.agent_id, "state": cert_state(c, grace, now_ms),
                    "not_after": crate::api::http::ts_ms(c.not_after), "issued_via": c.issued_via,
                    "revoked_at": c.revoked_at.map(crate::api::http::ts_ms), "revocation_reason": c.revocation_reason,
                })).collect::<Vec<_>>()}),
                ));
            }
            let mut t = vec![vec![
                "序號".to_string(),
                "AGENT_ID".into(),
                "狀態".into(),
                format!("到期（{}）", zone_label(env.tz)),
                "來源".into(),
            ]];
            for c in &shown {
                t.push(vec![
                    c.serial.clone(),
                    c.agent_id.clone(),
                    cert_state(c, grace, now_ms).into(),
                    human_time(c.not_after, env.tz),
                    c.issued_via.clone(),
                ]);
            }
            Ok(Output {
                stdout: format!("{}\n共 {} 張\n", table(&t), shown.len()),
                ..Default::default()
            })
        }
        (Some("revoke"), _) => {
            let reason = cmd.opt("--reason").and_then(Reason::parse).ok_or_else(|| usage_err("--reason 必須是 keyCompromise、cessationOfOperation、superseded 或 unspecified"))?;
            let comment = cmd.opt("--comment").unwrap_or("").to_string();
            let mut conn = db(cmd)?;
            let mut serials = Vec::new();
            for target in &cmd.args[1..] {
                if kairos_proto::is_uuid_text(target) {
                    let id = target.to_ascii_lowercase();
                    serials.extend(
                        ps::certs_of_agent(&conn, &id)
                            .map_err(db_failure)?
                            .into_iter()
                            .filter(|c| c.revoked_at.is_none() && c.not_after > now_ms)
                            .map(|c| c.serial),
                    );
                } else {
                    serials.push(target.to_ascii_uppercase());
                }
            }
            if serials.is_empty() {
                return Err(usage_err("沒有要撤銷的憑證"));
            }
            if cmd.flag("--yes") {
                let expect = int_opt(cmd, "--expect-count")?.ok_or_else(|| {
                    usage_err("非互動執行時需要 --yes 與 --expect-count <N>（L2）")
                })?;
                if expect as usize != serials.len() {
                    return Err(Failure::new(
                        "未撤銷",
                        format!("--expect-count {expect} 與實際張數 {} 不同", serials.len()),
                        Vec::new(),
                        EXIT_USAGE,
                    ));
                }
            } else {
                let want = format!("撤銷 {} 張", serials.len());
                let typed = (env.confirm)(&format!(
                    "即將撤銷 {} 張憑證（{}）：使用中的主機會在數秒內斷線，且無法復原。\n請輸入「{want}」確認：",
                    serials.len(),
                    serials.join("、")
                ));
                if typed.as_deref().map(str::trim) != Some(want.as_str()) {
                    return Err(Failure::new(
                        "未撤銷",
                        "確認字串不符（非互動執行時需要 --yes 與 --expect-count）",
                        Vec::new(),
                        EXIT_USAGE,
                    ));
                }
            }
            if comment.trim().is_empty() {
                return Err(usage_err("--comment 必填（寫入稽核）"));
            }
            let results = ops::revoke(
                &mut conn,
                &serials,
                reason,
                &comment,
                &operator(),
                None,
                now_ms,
            )
            .map_err(op_failure)?;
            let failed = results.iter().any(|(_, r)| *r == RevokeResult::NotFound);
            if cmd.json {
                let mut o = ok_json(
                    cmd,
                    env.now,
                    json!({"results": results.iter().map(|(s, r)| json!({"serial": s, "result": match r { RevokeResult::Revoked { .. } => "revoked", RevokeResult::Already => "already_revoked", RevokeResult::NotFound => "not_found" }})).collect::<Vec<_>>()}),
                );
                o.exit = if failed { EXIT_GENERAL } else { 0 };
                return Ok(o);
            }
            let mut text = String::new();
            for (s, r) in results {
                text.push_str(&format!("{} {s}：{}\n", mark(env), match r { RevokeResult::Revoked { .. } => "已撤銷（執行中的 Kairos 在 2 秒內更新 CRL 並斷開連線；離線模式需上傳 CRL）", RevokeResult::Already => "先前已撤銷", RevokeResult::NotFound => "找不到" }));
            }
            Ok(Output {
                stdout: text,
                exit: if failed { EXIT_GENERAL } else { 0 },
                ..Default::default()
            })
        }
        (Some("queue"), Some("export")) => {
            let config = load_config(cmd.config_path.as_deref())?;
            let conn = db(cmd)?;
            let overrides =
                settings::overrides_from(&store::load_settings(&conn).map_err(db_failure)?);
            let days = settings::effective(&config, &overrides)
                .policy
                .cert_lifetime_days;
            let v = crate::pki::queue::export(&conn, "kairos", days, now_ms).map_err(db_failure)?;
            let text = serde_json::to_string_pretty(&v).unwrap_or_default() + "\n";
            let n = v["items"].as_array().map_or(0, Vec::len);
            match cmd.opt("--out") {
                Some(out) => {
                    write_new_file(out, text.as_bytes())?;
                    Ok(Output {
                        stdout: if cmd.json {
                            ascii_json(
                                &json!({"schema": cmd.schema(), "ok": true, "data": {"items": n, "out": out}}),
                            ) + "\n"
                        } else {
                            String::new()
                        },
                        stderr: if cmd.json {
                            String::new()
                        } else {
                            format!("{} 已匯出 {n} 筆待簽發的申請到 {out}\n", mark(env))
                        },
                        exit: 0,
                    })
                }
                None => Ok(Output {
                    stdout: text,
                    stderr: format!("已匯出 {n} 筆待簽發的申請\n"),
                    exit: 0,
                }),
            }
        }
        (Some("queue"), Some("import")) => {
            let path = cmd
                .sub(2)
                .ok_or_else(|| usage_err("需要 kairos-signed-certs/v1 的檔案"))?;
            let config = load_config(cmd.config_path.as_deref())?;
            let v: Value = serde_json::from_str(&read_file(path)?)
                .map_err(|e| usage_err(format!("{path} 不是 JSON：{e}")))?;
            let mut conn = db(cmd)?;
            let anchors = crate::pki::queue::anchors(&conn, config.auth.client_ca_path());
            let results = crate::pki::queue::import(&mut conn, &v, &anchors, &operator(), now_ms)
                .map_err(usage_err)?;
            let failed = results.iter().any(|r| r.result.is_err());
            if cmd.json {
                let mut o = ok_json(
                    cmd,
                    env.now,
                    json!({"results": results.iter().map(|r| match &r.result { Ok(s) => json!({"request_id": r.request_id, "result": "issued", "serial": s}), Err(e) => json!({"request_id": r.request_id, "result": "rejected", "reason": e}) }).collect::<Vec<_>>()}),
                );
                o.exit = if failed { EXIT_CERT } else { 0 };
                return Ok(o);
            }
            let mut text = String::new();
            for r in &results {
                match &r.result {
                    Ok(s) => text.push_str(&format!(
                        "{} {}：已匯入（序號 {s}）\n",
                        mark(env),
                        r.request_id
                    )),
                    Err(e) => text.push_str(&format!(
                        "{} {}：不通過：{e}\n",
                        if env.unicode { "✗" } else { "[FAIL]" },
                        r.request_id
                    )),
                }
            }
            Ok(Output {
                stdout: text,
                exit: if failed { EXIT_CERT } else { 0 },
                ..Default::default()
            })
        }
        (Some(s), _) => Err(usage_err(format!("無法辨識的子命令: {s}"))),
        (None, _) => Err(usage_err("需要子命令")),
    }
}

fn cert_state(c: &ps::CertRow, grace_days: u32, now_ms: i64) -> &'static str {
    if c.revoked_at.is_some() {
        if c.revoked_effective(now_ms) {
            "revoked"
        } else {
            "revocation_pending"
        }
    } else if c.not_after <= now_ms {
        if now_ms < c.not_after + i64::from(grace_days) * 86_400_000 {
            "expired_in_grace"
        } else {
            "expired_beyond_grace"
        }
    } else if c.not_after - now_ms <= 30 * 86_400_000 {
        "expiring"
    } else {
        "valid"
    }
}

/// `kairos pki sign-bundle`：在離線的 CA 主機上簽發（不讀 Kairos 的設定與資料庫）
fn sign_bundle(cmd: &Cmd, env: &Env<'_>) -> Result<Output, Failure> {
    let input = cmd.opt("--in").ok_or_else(|| usage_err("--in 必填"))?;
    let ca_cert = cmd
        .opt("--ca-cert")
        .ok_or_else(|| usage_err("--ca-cert 必填"))?;
    let ca_key = cmd
        .opt("--ca-key")
        .ok_or_else(|| usage_err("--ca-key 必填"))?;
    let out = cmd.opt("--out").ok_or_else(|| usage_err("--out 必填"))?;
    if Path::new(out).exists() {
        return Err(Failure::new(
            format!("{out} 已存在"),
            "輸出檔已存在時不覆寫",
            Vec::new(),
            EXIT_CONFLICT,
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(ca_key)
            && m.permissions().mode() & 0o077 != 0
        {
            return Err(cert_fail(format!(
                "私鑰 {ca_key} 的權限過寬（{:04o}），應為 0600",
                m.permissions().mode() & 0o7777
            )));
        }
    }
    let queue: Value = serde_json::from_str(&read_file(input)?)
        .map_err(|e| usage_err(format!("{input} 不是 JSON：{e}")))?;
    let cert_pem = read_file(ca_cert)?;
    let der = crate::pki::x509::pem_decode_all(&cert_pem, Some("CERTIFICATE"))
        .map_err(cert_fail)?
        .into_iter()
        .next()
        .ok_or_else(|| cert_fail("找不到 CA 憑證"))?
        .1;
    let ca = crate::pki::issue::IssuingCa::load(0, &der, &read_file(ca_key)?).map_err(cert_fail)?;
    let signed = crate::pki::queue::sign_bundle(&queue, &ca, env.now.timestamp_millis())
        .map_err(usage_err)?;
    write_new_file(
        out,
        (serde_json::to_string_pretty(&signed).unwrap_or_default() + "\n").as_bytes(),
    )?;
    let n = signed["items"].as_array().map_or(0, Vec::len);
    let e = signed["errors"].as_array().map_or(0, Vec::len);
    if cmd.json {
        return Ok(ok_json(
            cmd,
            env.now,
            json!({"signed": n, "errors": signed["errors"], "out": out}),
        ));
    }
    Ok(Output {
        stdout: format!(
            "{} 已簽發 {n} 張，寫入 {out}{}\n",
            mark(env),
            if e > 0 {
                format!("；{e} 筆 CSR 不合法（見檔案中的 errors）")
            } else {
                String::new()
            }
        ),
        exit: if e > 0 { EXIT_GENERAL } else { 0 },
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// 來源的寫法（`cli-style.md` §11.7）
fn source_text(v: &settings::SettingView, tz: FixedOffset) -> String {
    match v.source {
        SettingSource::Environment => format!("環境變數 {}", v.env_var),
        SettingSource::Console => format!(
            "主控台（{}，{}）",
            v.updated_by.as_deref().unwrap_or("-"),
            v.updated_at.map(|t| human_time(t, tz)).unwrap_or_default()
        ),
        SettingSource::File => "設定檔".into(),
        SettingSource::Default => "內建預設".into(),
    }
}

fn config_cmd(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    match cmd.sub(0) {
        Some("show") => config_show(cmd, env),
        Some("reset") => config_reset(cmd, env),
        _ => Err(usage_err(
            "用法：kairos config show --effective、kairos config reset <設定鍵>...",
        )),
    }
}

fn config_show(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    if !cmd.flag("--effective") {
        return Err(usage_err(
            "用法：kairos config show --effective [<設定鍵或前綴>...] [--source env|console|file|default]",
        ));
    }
    let config = load_config(cmd.config_path.as_deref())?;
    let rows = match open(cmd.config_path.as_deref(), false) {
        Ok(Some(conn)) => store::load_settings(&conn).map_err(db_failure)?,
        Ok(None) => Vec::new(),
        Err(f) => return Err(f),
    };
    let filters: Vec<&str> = cmd.args[1..].iter().map(String::as_str).collect();
    let source = cmd.opt("--source");
    if let Some(s) = source
        && !matches!(s, "env" | "console" | "file" | "default")
    {
        return Err(usage_err(format!(
            "--source 的值 \"{s}\" 無效（env、console、file、default）"
        )));
    }
    let all = settings::effective_view(&config, &rows);
    for f in &filters {
        if !all
            .iter()
            .any(|v| v.key == *f || v.key.starts_with(&format!("{f}.")))
        {
            return Err(usage_err(format!("不認得的設定鍵或前綴：{f}")));
        }
    }
    let shown: Vec<&settings::SettingView> = all
        .iter()
        .filter(|v| {
            filters.is_empty()
                || filters
                    .iter()
                    .any(|f| v.key == *f || v.key.starts_with(&format!("{f}.")))
        })
        .filter(|v| source.is_none_or(|s| settings::short_source(v.source) == s))
        .collect();
    if cmd.json {
        return Ok(Output {
            stdout: ascii_json(&json!({
                "schema": "kairos.config.effective/v1", "ok": true, "generated_at": fmt_time(env.now),
                "data": {"settings": shown.iter().map(|v| v.json()).collect::<Vec<_>>()},
                "warnings": [], "error": null,
            })) + "\n",
            ..Default::default()
        });
    }
    let mut t = vec![vec![
        "設定鍵".to_string(),
        "生效值".into(),
        "來源".into(),
        "被覆寫的值".into(),
    ]];
    for v in &shown {
        let over = if v.overridden.is_empty() {
            "—".to_string()
        } else {
            v.overridden
                .iter()
                .map(|(s, val)| {
                    format!(
                        "{}：{val}",
                        match s {
                            SettingSource::Console => "主控台",
                            SettingSource::File => "設定檔",
                            SettingSource::Environment => "環境變數",
                            SettingSource::Default => "內建預設",
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("、")
        };
        t.push(vec![
            v.key.clone(),
            v.value.to_string(),
            source_text(v, env.tz),
            over,
        ]);
    }
    let console = shown
        .iter()
        .filter(|v| v.source == SettingSource::Console)
        .count();
    let envs = shown
        .iter()
        .filter(|v| v.source == SettingSource::Environment)
        .count();
    let mut out = table(&t);
    if console + envs > 0 {
        out.push_str(&format!(
            "\n{} {console} 個設定由主控台管理，優先於 kairos.toml；{envs} 個由環境變數決定（主控台無法修改）。\n  要改回設定檔的值：在主控台該欄位按「改用設定檔的值」，或執行 kairos config reset <設定鍵>。\n",
            if env.unicode { "ⓘ" } else { "[INFO]" }
        ));
    }
    Ok(Output {
        stdout: out,
        ..Default::default()
    })
}

fn config_reset(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let keys: Vec<String> = cmd.args[1..].to_vec();
    if keys.is_empty() {
        return Err(usage_err(
            "需要至少一個設定鍵，例如 kairos config reset enroll.mode",
        ));
    }
    let now_ms = env.now.timestamp_millis();
    let config = load_config(cmd.config_path.as_deref())?;
    let mut conn = db(cmd)?;
    // 改回設定檔的值可能等於關閉註冊端點或縮短寬限期：與 enroll mode closed、主控台相同的 L2
    // （設計包 screens.md §0.3；API POST /api/v1/settings/reset 的 impact_token）
    let overrides = settings::overrides_from(&store::load_settings(&conn).map_err(db_failure)?);
    let before = settings::effective(&config, &overrides);
    let mut rest = overrides.clone();
    for k in &keys {
        rest.remove(k);
    }
    let after = settings::effective(&config, &rest);
    let impact = ops::change_impact(&conn, &before, &after, now_ms).map_err(op_failure)?;
    let (level, confirm_text) = impact.level();
    if level == "L2" {
        let n = impact.count();
        if cmd.opt("--reason").is_none() {
            return Err(usage_err(format!(
                "改用設定檔的值會讓 {n} 台主機無法自動續簽（L2）：--reason 必填"
            )));
        }
        if cmd.flag("--yes") {
            let expect = int_opt(cmd, "--expect-count")?
                .ok_or_else(|| usage_err("非互動執行時需要 --yes --expect-count <N>（L2）"))?;
            if expect != n {
                return Err(Failure::new(
                    "未變更",
                    format!("影響台數已改變：目前 {n} 台（--expect-count {expect}）"),
                    vec!["確認數字後以新的 --expect-count 重新執行。".into()],
                    EXIT_REJECTED,
                ));
            }
        } else {
            let want = confirm_text.unwrap_or_default();
            let mut what = Vec::new();
            if let Some((from, i)) = &impact.closing {
                what.push(format!(
                    "註冊端點模式 {} → closed：寬限期內 {} 台、30 天內進入寬限期且離線 {} 台將無法自動續簽",
                    from.as_str(),
                    i.n,
                    i.m
                ));
            }
            if let Some(g) = &impact.grace {
                what.push(format!(
                    "過期寬限期 {} 天 → {} 天：{} 台立即無法續簽",
                    g.from_days, g.to_days, g.k
                ));
            }
            let typed = (env.confirm)(&format!(
                "{}。
請輸入「{want}」確認：",
                what.join("；")
            ));
            if typed.as_deref().map(str::trim) != Some(want.as_str()) {
                return Err(Failure::new(
                    "未變更",
                    "確認字串不符（非互動執行時需要 --yes --expect-count）",
                    Vec::new(),
                    EXIT_USAGE,
                ));
            }
        }
    } else if !cmd.flag("--yes") {
        let ok = (env.confirm)(&format!("即將刪除主控台對 {} 的設定，改用 kairos.toml（或內建預設）的值；執行中的 Kairos 在 2 秒內套用。
是否繼續？[y/N] ", keys.join("、")))
            .is_some_and(|a| matches!(a.trim(), "y" | "Y" | "yes"));
        if !ok {
            return Err(Failure::new(
                "未變更",
                "沒有確認（非互動執行時需要 --yes）",
                Vec::new(),
                EXIT_USAGE,
            ));
        }
    }
    let confirmed = impact.required().then_some(ops::Confirmed {
        level,
        reason: cmd.opt("--reason"),
        impact: &impact,
    });
    let results = ops::reset_settings(
        &mut conn,
        &keys,
        &operator(),
        None,
        confirmed.as_ref(),
        now_ms,
    )
    .map_err(op_failure)?;
    if cmd.json {
        return Ok(Output {
            stdout: ascii_json(&json!({
                "schema": "kairos.config.reset/v1", "ok": true, "generated_at": fmt_time(env.now),
                "data": {"results": results.iter().map(|(k, e)| json!({"key": k, "was_console": e})).collect::<Vec<_>>()},
                "warnings": [], "error": null,
            })) + "\n",
            ..Default::default()
        });
    }
    let mut text = String::new();
    for (k, existed) in results {
        text.push_str(&format!(
            "{} {k}：{}\n",
            mark(env),
            if existed {
                "已改用設定檔的值"
            } else {
                "原本就不是由主控台管理"
            }
        ));
    }
    Ok(Output {
        stdout: text,
        ..Default::default()
    })
}

/// 執行，回傳結束代碼
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
    fn arguments_are_parsed_into_positionals_options_and_flags() {
        let c = parse_args(
            Group::Enroll,
            os(&[
                "approve",
                "1KGJ-N9XN",
                "--yes",
                "--cert-days",
                "400",
                "--config=k.toml",
                "--json",
            ]),
        )
        .unwrap();
        assert_eq!(c.args, ["approve", "1KGJ-N9XN"]);
        assert!(c.flag("--yes") && c.json);
        assert_eq!(c.opt("--cert-days"), Some("400"));
        assert_eq!(c.config_path, Some(PathBuf::from("k.toml")));
        assert_eq!(c.schema(), "kairos.enroll.approve/v1");
        let c = parse_args(Group::Pki, os(&["issuing-ca", "new"])).unwrap();
        assert_eq!(c.schema(), "kairos.pki.issuing-ca.new/v1");
        let c = parse_args(
            Group::EnrollToken,
            os(&["create", "--tag", "a", "--tag", "b", "--name", "x"]),
        )
        .unwrap();
        assert_eq!(c.opts_all("--tag"), ["a", "b"]);
        assert!(parse_args(Group::Enroll, os(&[])).is_err());
        assert!(parse_args(Group::Enroll, os(&["list", "--token"])).is_err());
        assert!(parse_args(Group::Enroll, os(&["--help"])).unwrap().help);
    }
}
