//! `pulse enroll`（設計包 `pulse/cli-spec.md` §5）與安裝程式使用的 `pulse decommission`。
//!
//! 步驟與進度寫到 stderr，結果摘要寫到 stdout（`cli-style.md` §2）；`--json` 時 stdout 只有
//! `pulse.enroll/v1` 文件。不原地重繪 spinner（等同 `--progress plain`），螢幕報讀與部署工具都能逐行讀取。
//!
//! 結束代碼（`cli-spec.md` §1.3）：0 已註冊並連上；7 等待中（服務會在背景接續）；8 有警告；
//! 124 註冊端點忙碌、`--wait` 到了仍未送出且沒有安裝服務；2 用法錯誤（含 token 檢查碼）；
//! 4 本機憑證或私鑰問題；5 無法連線或伺服器身分驗證失敗；6 被拒絕；11 已註冊或已有進行中的註冊。

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kairos_proto::enroll::{
    self as ke, EnrollToken, InitiatedBy, InstallString, ServerVerification, TokenCheckRequest,
    WithdrawReason,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::client::{ApiError, EnrollClient, system_now};
use super::flow::{self, Driver, Event, Finish, Observer, RequestInfo, RunOutcome};
use super::store::{EnrollmentFile, FileKind, IdentityPaths, state};
use super::toml_edit::{self, Value as T};
use super::trust::TrustSpec;
use super::{NewEnrollment, prepare, store, x509};
use crate::cli::UsageError;
use crate::codes;
use crate::config_manager::AppConfig;
use crate::status_cmd::term::{self, Style};

/// `pulse enroll --help`
pub const USAGE: &str = "\
pulse enroll — 以註冊 token 向 Kairos 註冊這台主機

用法：
  pulse enroll --server <host[:port]> --token <token> [--ca-pin <sha256:…> | --ca-file <路徑>]
               [--enroll-server <host[:port] | :port>] [--name <顯示名稱>] [--tag <標籤>]...
               [--wait <秒> | --no-wait] [--force] [--reset-identity] [--trust-on-first-use]
  pulse enroll --install-string <字串> [...]
  pulse enroll --token-file <路徑|-> [...]
  pulse enroll --status            顯示目前的註冊狀態
  pulse enroll --cancel            撤回進行中的申請並刪除本機未使用的私鑰（L1）

說明：
  在本機產生私鑰（ECDSA P-256，不會離開這台主機）與 CSR，經註冊端點（預設與 --server 相同主機的
  8444 連接埠）送出申請；核准後取得憑證、寫入 identity/ 與 pulse.toml。中斷後重新執行會從上次的
  申請接續，不會產生第二筆申請。只信任 CA 指紋、--ca-file 或人工確認的 CA，不使用作業系統信任庫。

選項：
      --server <host[:port]>          Kairos 的 agent 連線位址（連接埠預設 8443）
      --enroll-server <host[:port]>   註冊端點；只改連接埠時寫 :port（預設 <server 主機>:8444）
      --token <token>                 註冊 token（KRT1-XXXX-XXXX-XXXX-XXXX，不分大小寫、連字號可省略）
      --token-file <路徑|->           從檔案（- 為 stdin）讀取 token
      --install-string <字串>         安裝字串（pulse-enroll:v1:…，含位址、token 與 CA 指紋）
      --ca-pin <sha256:…>             Kairos CA 的指紋
      --ca-file <路徑>                Kairos CA 憑證（PEM）
      --trust-on-first-use            非互動模式下沒有指紋時信任註冊端點出示的 CA（未驗證伺服器身分）
      --name <顯示名稱>               主控台的顯示名稱
      --tag <標籤>                    標籤（可重複，例如 --tag site:tainan）
      --wait <秒>                     等待核准的時間（TTY 預設 600，非 TTY 預設 0）；時間到服務會繼續等待
      --no-wait                       送出後立即返回（結束代碼 7）
      --force                         已註冊時重新註冊（沿用 agent_id、產生新私鑰；L1）
      --reset-identity                產生新的 agent_id 後註冊（複製的 VM；L2）
      --status                        顯示目前的註冊狀態
      --cancel                        撤回進行中的申請（L1）
  -y, --yes                           不詢問（危險操作在非互動模式必須加上）
      --non-interactive               不詢問；缺少參數時直接失敗
      --reason <原因>                 危險操作的原因（寫入日誌）
      --json                          輸出機器可讀的 JSON（schema pulse.enroll/v1）
  -c, --config <路徑>                 pulse.toml 的位置（identity/ 等相對路徑以它所在的目錄解析）
      --ascii                         不使用 Unicode 符號
  -q, --quiet                         只輸出錯誤
  -h, --help                          顯示此說明

範例：
  sudo pulse enroll --install-string pulse-enroll:v1:eyJzIjoia2Fpcm9z…
  sudo pulse enroll --server kairos.example.gov.tw --token KRT1-7G4Q-M2XD-9PBA-H6WN \\
                    --ca-pin sha256:3f9a1c07b2d48e610a9c55f27d13c8a0e4b729610f3d8ac291e56b04d7afe21b

結束代碼：
  0 已註冊  7 等待核准或排隊中（服務會在背景接續）  8 有警告  2 用法錯誤  4 本機憑證錯誤
  5 無法連線或伺服器身分驗證失敗  6 被拒絕  11 已註冊或已有進行中的註冊  124 逾時";

/// `pulse enroll` 的參數
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnrollArgs {
    pub server: Option<String>,
    pub enroll_server: Option<String>,
    pub token: Option<String>,
    pub token_file: Option<String>,
    pub install_string: Option<String>,
    pub ca_pin: Option<String>,
    pub ca_file: Option<String>,
    pub name: Option<String>,
    pub tags: Vec<String>,
    pub wait: Option<u64>,
    pub no_wait: bool,
    pub force: bool,
    pub reset_identity: bool,
    pub tofu: bool,
    pub yes: bool,
    pub non_interactive: bool,
    pub reason: Option<String>,
    pub cancel: bool,
    pub status: bool,
    pub json: bool,
    pub config: Option<String>,
    pub ascii: bool,
    pub quiet: bool,
}

/// 解析 `pulse enroll` 之後的參數；`Ok(None)` 為 `--help`
pub fn parse(args: &[String]) -> Result<Option<EnrollArgs>, UsageError> {
    let err = |m: String| Err(UsageError::new(m, USAGE));
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(None);
    }
    let mut a = EnrollArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg, None),
        };
        let mut value = |what: &str| -> Result<String, UsageError> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| UsageError::new(format!("PLS-E0002：{name} 需要{what}"), USAGE))
        };
        let flag = inline.is_none();
        match name {
            "--server" => a.server = Some(value("位址")?),
            "--enroll-server" => a.enroll_server = Some(value("位址")?),
            "--token" => a.token = Some(value("token")?),
            "--token-file" => a.token_file = Some(value("路徑")?),
            "--install-string" => a.install_string = Some(value("安裝字串")?),
            "--ca-pin" => a.ca_pin = Some(value("CA 指紋")?),
            "--ca-file" => a.ca_file = Some(value("路徑")?),
            "--name" => a.name = Some(value("顯示名稱")?),
            "--tag" => a.tags.push(value("標籤")?),
            "--wait" => {
                let v = value("秒數")?;
                match v.parse::<u64>() {
                    Ok(n) if n <= 86_400 => a.wait = Some(n),
                    _ => {
                        return err(format!(
                            "PLS-E0002：--wait 必須是 0～86400 的整數（目前為「{v}」）"
                        ));
                    }
                }
            }
            "--reason" => a.reason = Some(value("原因")?),
            "-c" | "--config" => a.config = Some(value("設定檔路徑")?),
            "--no-wait" if flag => a.no_wait = true,
            "--force" if flag => a.force = true,
            "--reset-identity" if flag => a.reset_identity = true,
            "--trust-on-first-use" if flag => a.tofu = true,
            "-y" | "--yes" if flag => a.yes = true,
            "--non-interactive" if flag => a.non_interactive = true,
            "--cancel" if flag => a.cancel = true,
            "--status" if flag => a.status = true,
            "--json" if flag => a.json = true,
            "--ascii" if flag => a.ascii = true,
            "-q" | "--quiet" if flag => a.quiet = true,
            "-v" | "--verbose" | "--no-color" if flag => {}
            "--color" | "--progress" => {
                value("模式")?;
            }
            other => {
                return err(format!(
                    "PLS-E0001：pulse enroll 未知的參數「{other}」（使用 pulse enroll --help 查看用法）"
                ));
            }
        }
        i += 1;
    }
    let exclusive = [
        (a.wait.is_some() && a.no_wait, "--wait 與 --no-wait"),
        (
            a.ca_pin.is_some() && a.ca_file.is_some(),
            "--ca-pin 與 --ca-file",
        ),
        (
            a.token.is_some() && a.token_file.is_some(),
            "--token 與 --token-file",
        ),
        (a.cancel && a.status, "--cancel 與 --status"),
        (
            (a.cancel || a.status) && (a.token.is_some() || a.install_string.is_some()),
            "--cancel／--status 與 token",
        ),
    ];
    if let Some((_, what)) = exclusive.iter().find(|(bad, _)| *bad) {
        return err(format!("PLS-E0003：{what} 不能同時使用"));
    }
    Ok(Some(a))
}

// ---------------------------------------------------------------------------
// 輸出
// ---------------------------------------------------------------------------

/// 一則問題（`--json` 的 `error`／`data.issues[]` 與三段式）
#[derive(Debug, Clone)]
struct Issue {
    code: &'static str,
    severity: &'static str,
    title: String,
    cause: String,
    fix: Vec<String>,
    details: Value,
}

impl Issue {
    fn new(
        code: &'static str,
        severity: &'static str,
        cause: impl Into<String>,
        fix: Vec<String>,
    ) -> Self {
        Self {
            code,
            severity,
            title: codes::title(code).unwrap_or("註冊無法完成").to_string(),
            cause: cause.into(),
            fix,
            details: Value::Null,
        }
    }

    fn title(mut self, t: impl Into<String>) -> Self {
        self.title = t.into();
        self
    }

    fn details(mut self, d: Value) -> Self {
        self.details = d;
        self
    }

    fn json(&self) -> Value {
        json!({
            "code": self.code,
            "severity": self.severity,
            "title": self.title,
            "cause": self.cause,
            "fix": self.fix,
            "details": self.details,
            "explain": format!("pulse explain {}", self.code),
        })
    }
}

struct Out {
    style: Style,
    json: bool,
    quiet: bool,
    total: u8,
}

impl Out {
    fn sym(&self, kind: char) -> &'static str {
        match (self.style.unicode, kind) {
            (true, 'o') => "✓",
            (true, 'w') => "⚠",
            (true, 'x') => "✗",
            (true, 'p') => "…",
            (true, _) => "-",
            (false, 'o') => "[ OK ]",
            (false, 'w') => "[WARN]",
            (false, 'x') => "[FAIL]",
            (false, 'p') => "[WAIT]",
            (false, _) => "[SKIP]",
        }
    }

    fn step(&self, n: u8, kind: char, text: &str) {
        if self.json || (self.quiet && kind != 'x') {
            return;
        }
        if self.style.unicode {
            eprintln!("[{n}/{}] {} {text}", self.total, self.sym(kind));
        } else {
            eprintln!("{} [{n}/{}] {text}", self.sym(kind), self.total);
        }
    }

    fn line(&self, text: &str) {
        if !self.json && !self.quiet {
            eprintln!("{text}");
        }
    }

    fn issue(&self, i: &Issue) {
        if self.json || (self.quiet && i.severity != "error") {
            return;
        }
        let (sym, label) = if i.severity == "error" {
            (self.sym('x'), "錯誤")
        } else {
            (self.sym('w'), "警告")
        };
        let mut s = format!(
            "\n{sym} {label} {}：{}\n  原因：{}\n",
            i.code, i.title, i.cause
        );
        if !i.fix.is_empty() {
            s.push_str("  處理方式：\n");
            for (n, f) in i.fix.iter().enumerate() {
                if i.fix.len() == 1 {
                    s.push_str(&format!("    {f}\n"));
                } else {
                    s.push_str(&format!("    {}. {f}\n", n + 1));
                }
            }
        }
        s.push_str(&format!("  說明：pulse explain {}", i.code));
        eprintln!("{s}");
    }
}

/// 一次執行的結果
struct Report {
    exit: u8,
    result: &'static str,
    error: Option<Issue>,
    issues: Vec<Issue>,
    data: Value,
    summary: Option<String>,
}

fn finish(out: &Out, r: Report) -> u8 {
    if out.json {
        let mut data = r.data;
        if let Some(obj) = data.as_object_mut() {
            obj.insert("result".into(), json!(r.result));
            obj.insert(
                "issues".into(),
                Value::Array(r.issues.iter().map(Issue::json).collect()),
            );
        }
        let doc = json!({
            "schema": "pulse.enroll/v1",
            "ok": r.error.is_none(),
            "generated_at": crate::status_file::ts(chrono::Utc::now()),
            "exit_code": r.exit,
            "data": data,
            "warnings": [],
            "error": r.error.as_ref().map(Issue::json),
        });
        print!("{}", crate::status_cmd::json::to_ascii_json(&doc));
    } else {
        for i in r.issues.iter().chain(r.error.iter()) {
            out.issue(i);
        }
        if let Some(s) = r.summary
            && !out.quiet
        {
            println!("{s}");
        }
    }
    r.exit
}

/// 錯誤代碼對應的結束代碼（`cli-spec.md` §1.3）
fn exit_for(code: &str) -> u8 {
    match code {
        codes::E_TOKEN_FORMAT
        | codes::E_NON_INTERACTIVE
        | codes::E_ARG_CONFLICT
        | codes::E_BAD_ARG_VALUE => 2,
        codes::E_KEY_MISMATCH | codes::E_CLIENT_CERT_LOAD | codes::E_CLIENT_CERT_MISMATCH => 4,
        codes::E_CA_PIN
        | codes::E_SERVER_CERT_UNTRUSTED
        | codes::E_SERVER_CERT_NAME
        | codes::E_SERVER_CERT_VALIDITY
        | codes::E_SERVER_CERT_REVOKED
        | codes::E_SERVER_CERT_PURPOSE
        | codes::E_ENROLL_UNSUPPORTED
        | codes::E_ENROLL_UNREACHABLE
        | codes::E_DNS
        | codes::E_CONN_REFUSED
        | codes::E_CONN_TIMEOUT
        | codes::E_SERVER_SILENT
        | codes::E_PER_IP_LIMIT => 5,
        codes::E_ALREADY_ENROLLED | codes::E_ENROLL_IN_PROGRESS => 11,
        codes::W_SERVER_UNEXPECTED => 1,
        _ => 6,
    }
}

/// 代碼的處理方式（三段式的第三段）
fn fix_for(code: &str, server_hint: &str) -> Vec<String> {
    let retry = format!("sudo pulse enroll --server {server_hint} --token <新 token>");
    match code {
        codes::E_TOKEN_INVALID | codes::E_TOKEN_EXPIRED | codes::E_TOKEN_EXHAUSTED => vec![
            "請 Kairos 管理者在主控台「註冊與憑證 › 註冊 token」建立新的 token。".into(),
            format!("以新 token 重新執行：{retry}"),
        ],
        codes::E_ENROLL_NOT_ACCEPTED => vec![
            "請 Kairos 管理者在「系統設定 › 連線與安全」把註冊端點改為「開放」，或建立新的 token（自動暫停時會自動恢復），之後再執行一次同一指令。".into(),
        ],
        codes::E_SOURCE_BLOCKED => vec![
            "確認使用的 token 正確（向 Kairos 管理者索取）。".into(),
            "需要立即註冊時，請管理者在「系統健康 › 註冊端點」解除封鎖，並提供錯誤代碼 PLS-E3005。".into(),
        ],
        codes::E_REQUEST_REJECTED => vec!["請向 Kairos 管理者確認拒絕原因；確認後以新的 token 執行 pulse enroll --force。".into()],
        codes::E_REQUEST_EXPIRED => vec!["執行 sudo pulse enroll --force 重新申請（token 仍有效時可沿用）。".into()],
        codes::E_CA_PIN => vec![
            "請 Kairos 管理者核對主控台「註冊與憑證 › 註冊 token」頁的 CA 指紋，確認安裝字串或 --ca-pin 正確。".into(),
            "若確實連到正確的 Kairos，請確認註冊端點的憑證鏈含有該 CA（ENROLL.md 第 2 節）。".into(),
        ],
        codes::E_ENROLL_UNREACHABLE => vec![
            "請 Kairos 管理者確認註冊端點（預設 8444）不是「關閉」，且防火牆允許這台主機連線。".into(),
        ],
        codes::E_ENROLL_UNSUPPORTED => vec!["升級 Kairos 到 0.5 以上，或改用手動憑證（INSTALL 第 5 節）。".into()],
        codes::E_LICENSE_EXHAUSTED => vec!["請 Kairos 管理者確認授權節點數。".into()],
        codes::E_CLOCK_SKEW => vec![
            if cfg!(windows) {
                "啟用時間同步並校正時間：w32tm /config /syncfromflags:domhier /update，再執行 net start w32time 與 w32tm /resync。".into()
            } else {
                "啟用時間同步：sudo timedatectl set-ntp true，並確認 timedatectl 顯示「System clock synchronized: yes」。".into()
            },
            "時間校正後，Pulse 服務會自動重新送出申請（沿用同一把私鑰），不需要新的 token，也不需要重新安裝。".into(),
        ],
        codes::E_ENROLL_RATE_LIMITED | codes::E_PENDING_QUEUE_FULL => vec![
            "本機不需處理，服務會在背景自動重試。若超過 30 分鐘仍未送出，請聯絡 Kairos 管理者並提供錯誤代碼與網路位址。".into(),
        ],
        codes::E_ALREADY_ENROLLED => vec!["要重新註冊（例如憑證被撤銷）請加上 --force；要除役請完全移除 Pulse。".into()],
        codes::E_ENROLL_IN_PROGRESS => vec![
            "查看進度：pulse enroll --status".into(),
            "改用新的申請：pulse enroll --cancel 之後再執行，或加上 --force。".into(),
        ],
        _ => vec!["查看日誌與 pulse status；仍無法解決時以 pulse diag 產生診斷包聯絡支援。".into()],
    }
}

// ---------------------------------------------------------------------------
// 執行
// ---------------------------------------------------------------------------

/// 執行 `pulse enroll`
pub fn run(args: &EnrollArgs) -> u8 {
    let style = term::detect(
        term::Stream::Stderr,
        term::ColorChoice::Never,
        args.ascii,
        args.json,
    );
    let out = Out {
        style,
        json: args.json,
        quiet: args.quiet,
        total: 6,
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("錯誤：無法建立執行環境：{e}");
            return 1;
        }
    };
    let report = rt.block_on(run_async(args, &out));
    finish(&out, report)
}

/// 設定檔與相對路徑的基準目錄
struct Located {
    config: AppConfig,
    config_path: PathBuf,
    base: PathBuf,
}

fn locate(args_config: Option<&str>) -> Result<Located, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("無法取得目前目錄：{e}"))?;
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let l = crate::status_cmd::locate(args_config.map(Path::new), &cwd, exe_dir.as_deref());
    let config_path = l
        .config
        .clone()
        .unwrap_or_else(|| l.base.join(crate::config_manager::LOCAL_CONFIG_FILE));
    let config = AppConfig::load_file(&config_path)
        .map_err(|e| format!("無法載入設定 {}：{e}", config_path.display()))?;
    Ok(Located {
        config,
        config_path,
        base: l.base,
    })
}

fn interactive(args: &EnrollArgs) -> bool {
    !args.yes
        && !args.non_interactive
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
}

fn ask(prompt: &str) -> String {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_string()
}

fn usage_report(code: &'static str, cause: String, fix: Vec<String>) -> Report {
    Report {
        exit: exit_for(code),
        result: "failed",
        error: Some(Issue::new(code, "error", cause, fix)),
        issues: Vec::new(),
        data: json!({}),
        summary: None,
    }
}

fn service_installed() -> bool {
    crate::status_cmd::service::query(None)
        .is_some_and(|s| s.state != crate::status_cmd::service::SvcState::NotInstalled)
}

fn service_running() -> bool {
    crate::status_cmd::service::query(None)
        .is_some_and(|s| s.state == crate::status_cmd::service::SvcState::Running)
}

/// 重新啟動服務（已註冊時以新憑證連線）；回傳是否成功
fn restart_service() -> bool {
    let unit = crate::status_cmd::service::default_unit();
    let ok = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if cfg!(windows) {
        let _ = ok("sc.exe", &["stop", unit]);
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            if crate::status_cmd::service::query(None)
                .is_some_and(|s| s.state != crate::status_cmd::service::SvcState::Running)
            {
                break;
            }
        }
        ok("sc.exe", &["start", unit])
    } else {
        ok("systemctl", &["try-restart", unit])
    }
}

/// 讀取 token：`--token`、`--token-file`（`-` 為 stdin）
fn read_token(args: &EnrollArgs) -> Result<Option<String>, String> {
    if let Some(t) = &args.token {
        return Ok(Some(t.clone()));
    }
    let Some(path) = &args.token_file else {
        return Ok(None);
    };
    let text = if path == "-" {
        let mut s = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut s)
            .map_err(|e| e.to_string())?;
        s
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("無法讀取 {path}：{e}"))?
    };
    Ok(Some(
        text.lines().next().unwrap_or_default().trim().to_string(),
    ))
}

/// `--server`：連接埠預設 8443
fn normalize_server(input: &str, default_port: u16) -> Result<String, String> {
    let t = input.trim();
    let with_port = if t.starts_with('[') {
        if t.contains("]:") {
            t.to_string()
        } else {
            format!("{t}:{default_port}")
        }
    } else if t.matches(':').count() > 1 {
        format!("[{t}]:{default_port}")
    } else if t.contains(':') {
        t.to_string()
    } else {
        format!("{t}:{default_port}")
    };
    ke::normalize_endpoint(&with_port).map_err(|e| format!("「{input}」{e}"))
}

/// `--enroll-server`：`:port` 只改連接埠
fn resolve_enroll_server(input: Option<&str>, server: &str) -> Result<String, String> {
    let host = ke::endpoint_host(server);
    match input.map(str::trim) {
        None | Some("") => super::endpoint_of(host, ke::DEFAULT_ENROLL_PORT),
        Some(p) if p.starts_with(':') => {
            let port: u16 = p[1..]
                .parse()
                .map_err(|_| format!("「{p}」不是合法的連接埠"))?;
            super::endpoint_of(host, port)
        }
        Some(e) => normalize_server(e, ke::DEFAULT_ENROLL_PORT),
    }
}

/// 申請的來源參數（安裝字串與旗標合併後）
struct Target {
    server: String,
    enroll_server: String,
    token: EnrollToken,
    pin: Option<String>,
    tags: Option<Vec<String>>,
}

#[allow(clippy::result_large_err)]
fn resolve_target(args: &EnrollArgs) -> Result<Target, Report> {
    let bad = |code: &'static str, cause: String| {
        usage_report(code, cause, fix_for(code, "<Kairos 位址>"))
    };
    let token_text = read_token(args).map_err(|e| bad(codes::E_BAD_ARG_VALUE, e))?;
    let decoded = match &args.install_string {
        Some(s) => Some(InstallString::decode(s).map_err(|e| {
            usage_report(
                codes::E_BAD_ARG_VALUE,
                format!("安裝字串無法解析：{e}"),
                vec!["請從主控台「註冊與憑證 › 註冊 token」重新複製完整的安裝字串（pulse-enroll:v1:…）。".into()],
            )
        })?),
        None => None,
    };
    let server_flag = args
        .server
        .as_deref()
        .map(|s| normalize_server(s, 8443))
        .transpose()
        .map_err(|e| bad(codes::E_BAD_ARG_VALUE, format!("--server {e}")))?;
    let pin_flag = args
        .ca_pin
        .as_deref()
        .map(ke::normalize_ca_pin)
        .transpose()
        .map_err(|e| bad(codes::E_BAD_ARG_VALUE, format!("--ca-pin：{e}")))?;
    let token_flag = token_text
        .as_deref()
        .map(EnrollToken::parse)
        .transpose()
        .map_err(|e| {
            usage_report(
                codes::E_TOKEN_FORMAT,
                format!(
                    "「{}」{e}；容易混淆的字元：V／W、2／Z、5／S、8／B。",
                    token_text.as_deref().unwrap_or("")
                ),
                vec!["請對照主控台或紙本重新輸入；大小寫、連字號都可以省略。".into()],
            )
        })?;
    let conflict = |what: &str| {
        usage_report(
            codes::E_ARG_CONFLICT,
            format!("{what} 與安裝字串的值不同"),
            vec!["只用安裝字串，或移除與它不同的旗標。".into()],
        )
    };
    let (server, enroll, token, pin, tags) = match decoded {
        Some(d) => {
            if server_flag.as_ref().is_some_and(|s| *s != d.s) {
                return Err(conflict("--server"));
            }
            if let (Some(p), true) = (&pin_flag, true)
                && *p != d.p
            {
                return Err(conflict("--ca-pin"));
            }
            let t =
                EnrollToken::parse(&d.t).map_err(|e| bad(codes::E_TOKEN_FORMAT, e.to_string()))?;
            if token_flag
                .as_ref()
                .is_some_and(|f| f.canonical() != t.canonical())
            {
                return Err(conflict("--token"));
            }
            let enroll_from_string = d.e.clone();
            if let (Some(flag), Some(e)) = (&args.enroll_server, &enroll_from_string) {
                let flag = resolve_enroll_server(Some(flag), &d.s)
                    .map_err(|x| bad(codes::E_BAD_ARG_VALUE, x))?;
                if flag != *e {
                    return Err(conflict("--enroll-server"));
                }
            }
            (d.s, enroll_from_string, t, Some(d.p), d.g)
        }
        None => {
            let Some(server) = server_flag else {
                return Err(usage_report(
                    codes::E_NON_INTERACTIVE,
                    "缺少 --server（或 --install-string）".into(),
                    vec!["例如：sudo pulse enroll --server kairos.example.gov.tw --token <token> --ca-pin <指紋>".into()],
                ));
            };
            let Some(token) = token_flag else {
                return Err(usage_report(
                    codes::E_NON_INTERACTIVE,
                    "缺少 --token（或 --token-file、--install-string）".into(),
                    vec!["請向 Kairos 管理者索取註冊 token。".into()],
                ));
            };
            (server, None, token, pin_flag, None)
        }
    };
    let enroll_server = match enroll {
        Some(e) => e,
        None => resolve_enroll_server(args.enroll_server.as_deref(), &server)
            .map_err(|e| bad(codes::E_BAD_ARG_VALUE, format!("--enroll-server {e}")))?,
    };
    let mut all_tags = tags.unwrap_or_default();
    all_tags.extend(args.tags.iter().cloned());
    Ok(Target {
        server,
        enroll_server,
        token,
        pin,
        tags: (!all_tags.is_empty()).then_some(all_tags),
    })
}

/// 進度輸出
struct CliObserver<'a> {
    out: &'a Out,
    shown_pending: bool,
    zone: crate::status_cmd::fmt::Zone,
}

impl CliObserver<'_> {
    fn at(&self, t: Option<&str>) -> String {
        t.and_then(crate::status_file::parse_ts)
            .map(|t| crate::status_cmd::fmt::date_time(t, self.zone))
            .unwrap_or_else(|| "?".into())
    }
}

impl Observer for CliObserver<'_> {
    fn on(&mut self, file: &EnrollmentFile, event: &Event) {
        let rc = file.request_code.as_deref().unwrap_or("?");
        match event {
            Event::Submitted => self.out.step(
                4,
                'o',
                &if file.state == state::PENDING_APPROVAL {
                    format!(
                        "已送出註冊申請（申請編號 {rc}，有效至 {}）",
                        self.at(file.expires_at.as_deref())
                    )
                } else {
                    format!("已送出註冊申請（申請編號 {rc}）")
                },
            ),
            Event::Pending { .. } if !self.shown_pending => {
                self.shown_pending = true;
                self.out.step(5, 'p', "等待管理者核准…");
                self.out.line(&format!(
                    "      請把申請編號 {rc} 告訴 Kairos 管理者，在主控台「註冊與憑證 › 待核准」核准。\n      按 Ctrl+C 可先離開，服務會繼續等待；之後再執行 pulse enroll 會接續等待。"
                ));
            }
            Event::Pending { .. } => {}
            Event::Retry {
                code,
                wait,
                rate_limited: true,
            } => self.out.step(
                4,
                'p',
                &format!("註冊端點忙碌，{} 秒後重試（{code}）", wait.as_secs()),
            ),
            Event::Retry { code, wait, .. } => self.out.step(
                4,
                'p',
                &format!("無法連線到註冊端點，{} 秒後重試（{code}）", wait.as_secs()),
            ),
            Event::ClockRejected { .. } => self.out.step(
                4,
                'w',
                &format!(
                    "註冊申請暫不受理：本機時鐘比 Kairos {} {}（PLS-E8302）",
                    if file.offset_ms.unwrap_or(0) < 0 {
                        "慢"
                    } else {
                        "快"
                    },
                    crate::status_cmd::fmt::offset_amount(file.offset_ms.unwrap_or(0))
                ),
            ),
            Event::Approved => self.out.step(
                5,
                'o',
                &format!(
                    "已核准（核准者：{}）",
                    file.decided_by.as_deref().unwrap_or("?")
                ),
            ),
            Event::Installed => self.out.step(
                6,
                'o',
                &format!(
                    "已安裝憑證（序號 {}，有效至 {}）",
                    file.cert_serial.as_deref().unwrap_or("?"),
                    self.at(file.cert_not_after.as_deref())
                ),
            ),
            Event::Stopped { code } => self.out.step(4, 'x', &format!("申請無法完成（{code}）")),
            Event::NotExpired => {}
        }
    }
}

fn file_json(file: &EnrollmentFile) -> Value {
    json!({
        "state": file.state,
        "request_code": file.request_code,
        "submitted_at": file.submitted_at,
        "expires_at": file.expires_at,
        "approved_by": file.decided_by.clone().filter(|_| matches!(file.state.as_str(), state::ISSUED | state::REGISTERED)),
        "rejected_reason": file.reject_reason,
        "offset_ms": file.offset_ms.filter(|_| file.state == state::CLOCK_REJECTED),
        "local_time": file.local_time.clone().filter(|_| file.state == state::CLOCK_REJECTED),
        "server_time": file.server_time.clone().filter(|_| file.state == state::CLOCK_REJECTED),
        "next_submit_at": file.next_submit_at,
        "last_code": file.last_code,
    })
}

async fn run_async(args: &EnrollArgs, out: &Out) -> Report {
    let located = match locate(args.config.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            return usage_report(
                codes::E_CONFIG_SYNTAX,
                e,
                vec!["修正設定檔後再執行。".into()],
            )
            .exit3();
        }
    };
    let paths = super::identity_paths(&located.config, &located.base);
    let Some(_lock) = (match paths.try_lock() {
        Ok(l) => l,
        Err(e) => {
            return usage_report(
                codes::E_NEED_ADMIN,
                format!("無法建立或鎖定 {}：{e}", paths.lock().display()),
                vec!["以系統管理員（sudo）執行。".into()],
            )
            .exit10();
        }
    }) else {
        return usage_report(
            codes::E_ENROLL_IN_PROGRESS,
            "另一個 pulse enroll 正在執行".into(),
            fix_for(codes::E_ENROLL_IN_PROGRESS, ""),
        );
    };
    let existing = paths.load_enrollment().ok().flatten();
    if args.status {
        return status(existing.as_ref(), &paths);
    }
    if args.cancel {
        return cancel(args, out, &paths, existing, &located).await;
    }
    let usable = {
        let c = located.config.server.client_cert.trim();
        !c.is_empty() && located.base.join(c).is_file()
    };
    let registered = usable
        || existing.as_ref().is_some_and(|f| {
            matches!(f.state.as_str(), state::ISSUED | state::REGISTERED)
                && paths.client_cert().is_file()
        });
    let new_request =
        args.token.is_some() || args.token_file.is_some() || args.install_string.is_some();
    // 接續進行中的申請
    if let Some(f) = existing.as_ref().filter(|f| {
        state::is_active(&f.state)
            && !f.is_grace()
            && !(f.state == state::ISSUED && f.certificate.is_none())
    }) {
        if !new_request && !args.force {
            return resume(args, out, &paths, f.clone(), &located).await;
        }
        if !args.force {
            return usage_report(
                codes::E_ENROLL_IN_PROGRESS,
                format!(
                    "已有進行中的註冊申請（{}，申請編號 {}）",
                    f.state,
                    f.request_code.as_deref().unwrap_or("尚未建立")
                ),
                fix_for(codes::E_ENROLL_IN_PROGRESS, ""),
            );
        }
    }
    if registered && !args.force && !args.reset_identity {
        return usage_report(
            codes::E_ALREADY_ENROLLED,
            "這台主機已註冊（已有用戶端憑證）".into(),
            fix_for(codes::E_ALREADY_ENROLLED, ""),
        );
    }
    if !new_request {
        return usage_report(
            codes::E_NON_INTERACTIVE,
            "缺少 --token、--token-file 或 --install-string".into(),
            vec!["例如：sudo pulse enroll --install-string <安裝字串>".into()],
        );
    }
    let target = match resolve_target(args) {
        Ok(t) => t,
        Err(r) => return r,
    };
    // 危險操作的確認（cli-spec §1.4）
    if (registered && args.force) || args.reset_identity {
        let level = if args.reset_identity { "L2" } else { "L1" };
        if !args.yes {
            if !interactive(args) {
                return usage_report(
                    codes::E_NON_INTERACTIVE,
                    format!(
                        "{} 是危險操作（{level}），非互動模式需要 --yes",
                        if args.reset_identity {
                            "--reset-identity"
                        } else {
                            "--force"
                        }
                    ),
                    vec!["加上 --yes（可加 --reason \"<原因>\"）後重新執行。".to_string()],
                );
            }
            let host = crate::system_info::hostname();
            let ok = if args.reset_identity {
                eprintln!(
                    "即將捨棄這台主機的身分並產生新的 agent_id；會刪除本機的私鑰與憑證，主控台上會出現一台新主機。這個操作無法復原。"
                );
                ask(&format!(
                    "請輸入主機名稱「{host}」以確認（按 Ctrl+C 取消）："
                )) == host
            } else {
                eprintln!(
                    "即將重新註冊這台主機：產生新的私鑰並以新的 token 申請；核准前目前的憑證仍會繼續使用。"
                );
                ask("? 是否繼續？[y/N]：").eq_ignore_ascii_case("y")
            };
            if !ok {
                return Report {
                    exit: 130,
                    result: "failed",
                    error: None,
                    issues: Vec::new(),
                    data: json!({}),
                    summary: Some("已取消，未做任何變更。".into()),
                };
            }
        }
        log::info!(
            "{}action={} level={level} reason={}",
            codes::tag(codes::ADMIN_DANGEROUS_ACTION, None),
            if args.reset_identity {
                "reset_identity"
            } else {
                "enroll_force"
            },
            args.reason.as_deref().unwrap_or("-")
        );
    }
    new_enrollment(args, out, &paths, &located, target, existing, registered).await
}

trait ExitOverride {
    fn exit3(self) -> Self;
    fn exit10(self) -> Self;
}

impl ExitOverride for Report {
    fn exit3(mut self) -> Self {
        self.exit = 3;
        self
    }
    fn exit10(mut self) -> Self {
        self.exit = 10;
        self
    }
}

fn status(file: Option<&EnrollmentFile>, paths: &IdentityPaths) -> Report {
    let Some(f) = file else {
        return Report {
            exit: 0,
            result: "status",
            error: None,
            issues: Vec::new(),
            data: json!({"enrollment": null}),
            summary: Some("目前沒有 pulse enroll 管理的註冊紀錄。".into()),
        };
    };
    let summary = format!(
        "{}\n  申請編號：{}\n  註冊端點：{}\n  enrollment.json：{}",
        super::service::status_text(f),
        f.request_code.as_deref().unwrap_or("—"),
        if f.enroll_server.is_empty() {
            "—"
        } else {
            &f.enroll_server
        },
        paths.enrollment().display()
    );
    Report {
        exit: 0,
        result: "status",
        error: None,
        issues: Vec::new(),
        data: json!({"enrollment": file_json(f)}),
        summary: Some(summary),
    }
}

async fn cancel(
    args: &EnrollArgs,
    out: &Out,
    paths: &IdentityPaths,
    existing: Option<EnrollmentFile>,
    located: &Located,
) -> Report {
    let Some(file) = existing
        .filter(|f| state::is_active(&f.state) && f.state != state::ISSUED && !f.is_grace())
    else {
        if located.config.server.client_cert.trim().is_empty() || !paths.client_cert().is_file() {
            return Report {
                exit: 0,
                result: "cancelled",
                error: None,
                issues: Vec::new(),
                data: json!({"withdrawal": {"result": "skipped"}}),
                summary: Some("目前沒有進行中的註冊申請，未做任何變更。".into()),
            };
        }
        return usage_report(
            codes::E_ALREADY_ENROLLED,
            "這台主機已註冊，--cancel 不適用（要除役請完全移除 Pulse）".into(),
            Vec::new(),
        );
    };
    let rc = file
        .request_code
        .clone()
        .unwrap_or_else(|| "（尚未建立）".into());
    if !args.yes {
        if !interactive(args) {
            return usage_report(
                codes::E_NON_INTERACTIVE,
                "--cancel 是危險操作（L1），非互動模式需要 --yes".into(),
                vec!["加上 --yes 後重新執行。".into()],
            );
        }
        eprintln!(
            "即將取消註冊申請 {rc}（{}）\n影響：\n  • 向 Kairos 撤回這筆申請；token 的使用次數不會退回。\n  • 刪除本機尚未使用的私鑰與申請紀錄；agent_id 保留。",
            file.state
        );
        if !ask("? 是否繼續？[y/N]：").eq_ignore_ascii_case("y") {
            return Report {
                exit: 130,
                result: "failed",
                error: None,
                issues: Vec::new(),
                data: json!({}),
                summary: Some("已取消，未做任何變更。".into()),
            };
        }
    }
    log::info!(
        "{}action=cancel level=L1 reason={}",
        codes::tag(codes::ADMIN_DANGEROUS_ACTION, None),
        args.reason.as_deref().unwrap_or("-")
    );
    let o = Out {
        style: out.style,
        json: out.json,
        quiet: out.quiet,
        total: 2,
    };
    let w = match super::service::trust_for(paths, &file, Some((&located.config, &located.base)))
        .and_then(|t| EnrollClient::new(&file.enroll_server, t, false, system_now()).ok())
    {
        Some(mut c) => {
            flow::withdraw(
                paths,
                &file,
                &mut c,
                WithdrawReason::EnrollCancelled,
                InitiatedBy::Cli,
            )
            .await
        }
        None => flow::Withdrawal {
            result: "unreported",
            code: Some(codes::W_WITHDRAW_UNREPORTED),
            reason: Some("unreachable"),
            cause_code: Some(codes::E_CA_PIN),
            protocol_code: None,
            request_code: file.request_code.clone(),
            request_state: None,
            attempts: 0,
            already_withdrawn: false,
        },
    };
    match w.result {
        "ok" => o.step(
            1,
            'o',
            &format!("已向 Kairos 撤回註冊申請 {rc}（{}）", file.enroll_server),
        ),
        "not_needed" => o.step(
            1,
            'o',
            &format!(
                "不需撤回：申請 {rc} 已{}",
                if w.request_state.as_deref() == Some("rejected") {
                    "被拒絕"
                } else {
                    "結束"
                }
            ),
        ),
        "skipped" => o.step(1, '-', "略過：Kairos 上沒有這台主機的申請"),
        _ => o.step(
            1,
            'w',
            &format!(
                "無法向 Kairos 撤回註冊申請 {rc}（嘗試 {} 次），繼續在本機取消",
                w.attempts
            ),
        ),
    }
    let _ = store::remove_if_exists(&paths.request_key());
    let _ = store::remove_if_exists(&paths.enrollment());
    o.step(
        2,
        'o',
        &format!(
            "已刪除 {} 與 enrollment.json（agent_id 保留）",
            paths.request_key().display()
        ),
    );
    let mut issues = Vec::new();
    if w.result == "unreported" {
        let why = match w.reason {
            Some("already_approved") => {
                "撤回前申請剛好被核准，Kairos 已簽發憑證，但這台主機沒有取回。".to_string()
            }
            Some("signature_rejected") => "Kairos 拒絕了撤回的簽章。".to_string(),
            Some("unsupported") => "Kairos 版本不支援撤回申請。".to_string(),
            _ => format!(
                "{} 無法連線（多半是註冊端點模式為「關閉」）。",
                file.enroll_server
            ),
        };
        let fix = if w.reason == Some("already_approved") {
            format!("請 Kairos 管理者在主控台把這台主機除役並撤銷剛簽發的憑證（申請 {rc}）。")
        } else {
            format!(
                "請 Kairos 管理者在主控台「註冊與憑證 › 待核准」拒絕申請 {rc}，或等它自動逾期。請不要核准：這台主機已沒有對應的私鑰。"
            )
        };
        log::warn!(
            "{}request_code={rc} reason={} cause_code={}",
            codes::tag(
                codes::INSTALL_WITHDRAWAL_UNREPORTED,
                Some(codes::W_WITHDRAW_UNREPORTED)
            ),
            w.reason.unwrap_or("-"),
            w.cause_code.unwrap_or("-")
        );
        issues.push(
            Issue::new(codes::W_WITHDRAW_UNREPORTED, "warning", format!("{why}本機的私鑰與申請紀錄已刪除，無法再重試。"), vec![fix])
                .title(format!("無法向 Kairos 撤回註冊申請 {rc}"))
                .details(json!({"reason": w.reason, "cause_code": w.cause_code, "protocol_code": w.protocol_code, "attempts": w.attempts})),
        );
    }
    let warn = !issues.is_empty();
    Report {
        exit: if warn { 8 } else { 0 },
        result: "cancelled",
        error: None,
        issues,
        data: json!({"withdrawal": withdrawal_json(&w)}),
        summary: Some(if warn {
            format!("已在本機取消註冊申請 {rc}，但有 1 個警告。")
        } else {
            format!(
                "已取消註冊申請 {rc}。之後要重新申請：sudo pulse enroll --server <位址> --token <token>"
            )
        }),
    }
}

fn withdrawal_json(w: &flow::Withdrawal) -> Value {
    json!({
        "result": w.result,
        "code": w.code,
        "reason": w.reason,
        "cause_code": w.cause_code,
        "request_code": w.request_code,
        "request_state": w.request_state,
    })
}

async fn resume(
    args: &EnrollArgs,
    out: &Out,
    paths: &IdentityPaths,
    file: EnrollmentFile,
    located: &Located,
) -> Report {
    out.line(&format!(
        "接續進行中的註冊申請（{}，申請編號 {}）",
        file.state,
        file.request_code.as_deref().unwrap_or("尚未建立")
    ));
    let Some(trust) = super::service::trust_for(paths, &file, None) else {
        return usage_report(
            codes::E_CA_PIN,
            "enrollment.json 沒有可用的信任來源".into(),
            vec!["執行 pulse enroll --cancel 後重新申請。".into()],
        );
    };
    let client = match EnrollClient::new(&file.enroll_server, trust, false, system_now()) {
        Ok(c) => c,
        Err(e) => return usage_report(codes::E_BAD_ARG_VALUE, e, Vec::new()),
    };
    let agent_id = file.agent_id.clone();
    drive(args, out, paths, located, file, client, &agent_id, None).await
}

#[allow(clippy::too_many_lines)]
async fn new_enrollment(
    args: &EnrollArgs,
    out: &Out,
    paths: &IdentityPaths,
    located: &Located,
    target: Target,
    existing: Option<EnrollmentFile>,
    registered: bool,
) -> Report {
    let config = &located.config;
    // ---- [1/6] 註冊端點與伺服器身分 ----
    let ca_file_certs = match &args.ca_file {
        Some(p) => match std::fs::read_to_string(p)
            .map_err(|e| e.to_string())
            .and_then(|t| x509::pem_certs(&t))
        {
            Ok(c) => Some(c),
            Err(e) => {
                return usage_report(
                    codes::E_BAD_ARG_VALUE,
                    format!("--ca-file {p}：{e}"),
                    Vec::new(),
                );
            }
        },
        None => None,
    };
    let (trust, verified_by) = match (&target.pin, &ca_file_certs) {
        (Some(p), _) => (TrustSpec::Pin(p.clone()), "ca_pin"),
        (None, Some(c)) => (TrustSpec::Anchors(c.clone()), "ca_file"),
        (None, None) if args.tofu => (TrustSpec::Tofu, "tofu_flag"),
        (None, None) if interactive(args) => (TrustSpec::Tofu, "tofu_interactive"),
        (None, None) => {
            return usage_report(
                codes::E_CA_PIN,
                "沒有提供 CA 指紋（--ca-pin、安裝字串）或 CA 檔（--ca-file），非互動模式無法確認連到的是真正的 Kairos".into(),
                vec!["改用安裝字串或加上 --ca-pin；確定要信任註冊端點出示的 CA 時加上 --trust-on-first-use（申請會標示「未驗證伺服器身分」）。".into()],
            );
        }
    };
    let mut client =
        match EnrollClient::new(&target.enroll_server, trust.clone(), false, system_now()) {
            Ok(c) => c,
            Err(e) => return usage_report(codes::E_BAD_ARG_VALUE, e, Vec::new()),
        };
    let info = match client.server_info().await {
        Ok(i) => i,
        Err(e) => return connect_failure(out, &target, e).await,
    };
    let tls = client
        .tls_version
        .clone()
        .unwrap_or_default()
        .replace("TLSv1_", "TLS 1.");
    let observed = client.observed();
    let mut issues = Vec::new();
    let mut trusted_pins: Vec<String> = Vec::new();
    match &trust {
        TrustSpec::Tofu => {
            let Some(anchor) = observed.anchor.clone() else {
                return usage_report(
                    codes::E_CA_PIN,
                    "註冊端點沒有送出 CA 憑證".into(),
                    Vec::new(),
                );
            };
            let pin = x509::parse_cert(&anchor)
                .map(|i| ke::ca_pin_from_spki(&i.spki))
                .unwrap_or_default();
            let display = ke::ca_pin_display(&pin).unwrap_or_default();
            if verified_by == "tofu_interactive" {
                eprintln!(
                    "註冊端點 {} 出示的 CA 指紋：\n    {}\n    {}\n請與主控台「註冊與憑證 › 註冊 token」頁的 CA 指紋核對。",
                    target.enroll_server, display[0], display[1]
                );
                if !ask("? 指紋相符，信任這個 CA？[y/N]：").eq_ignore_ascii_case("y") {
                    return usage_report(
                        codes::E_CA_PIN,
                        "指紋未確認，未送出任何資料".into(),
                        vec!["取得正確的 CA 指紋後以 --ca-pin 重新執行。".into()],
                    );
                }
            }
            out.step(
                1,
                'w',
                &format!(
                    "未提供 CA 指紋，已依 {} 信任此 CA：{} {}",
                    if verified_by == "tofu_flag" {
                        "--trust-on-first-use"
                    } else {
                        "人工確認"
                    },
                    display[0],
                    display[1]
                ),
            );
            issues.push(Issue::new(
                codes::W_TOFU,
                "warning",
                format!("沒有提供 CA 指紋或 CA 檔，已直接信任 {} 出示的 CA（指紋 {pin}）。若當時連到的是冒充的伺服器，這台主機會回報給錯誤的對象。", target.enroll_server),
                vec!["請 Kairos 管理者核對上方指紋與主控台的 CA 指紋；之後批次部署請改用 --ca-pin 或安裝字串。".into()],
            ));
            trusted_pins.push(pin);
        }
        TrustSpec::Pin(p) => {
            out.step(
                1,
                'o',
                &format!(
                    "連線到註冊端點 {}（{tls}），CA 指紋相符",
                    target.enroll_server
                ),
            );
            trusted_pins.push(p.clone());
        }
        TrustSpec::Anchors(c) => {
            out.step(
                1,
                'o',
                &format!(
                    "連線到註冊端點 {}（{tls}），以 --ca-file 驗證",
                    target.enroll_server
                ),
            );
            trusted_pins.extend(
                c.iter()
                    .filter_map(|c| x509::parse_cert(c))
                    .map(|i| ke::ca_pin_from_spki(&i.spki)),
            );
        }
    }
    if !info
        .body
        .enroll_api
        .iter()
        .any(|v| v == ke::ENROLL_API_VERSION)
    {
        return usage_report(
            codes::E_ENROLL_UNSUPPORTED,
            "註冊端點不支援註冊 API v1".into(),
            fix_for(codes::E_ENROLL_UNSUPPORTED, ""),
        );
    }
    if !info.body.accepts.new_enrollment {
        out.step(2, 'x', "送出註冊申請");
        return usage_report(
            codes::E_ENROLL_NOT_ACCEPTED,
            "Kairos 的註冊端點目前只接受憑證過期主機的自動續簽，以註冊 token 送出的新申請一律拒絕。token 本身沒有問題。本機尚未產生任何檔案，不需要清理。".into(),
            fix_for(codes::E_ENROLL_NOT_ACCEPTED, ""),
        );
    }
    if let Some(off) = client
        .last_offset_ms
        .filter(|o| o.abs() > 30_000 && o.abs() <= 300_000)
    {
        issues.push(Issue::new(
            codes::W_CLOCK_SKEW,
            "warning",
            format!("本機時鐘與 Kairos 相差 {}。差距超過 5 分鐘時，Kairos 會暫不受理註冊，憑證也可能被判定尚未生效或已過期。", crate::status_cmd::fmt::offset_amount(off)),
            fix_for(codes::E_CLOCK_SKEW, "")[..1].to_vec(),
        ));
    }
    // token-check（不消耗次數）：token 有問題時不產生任何檔案
    let check = client
        .token_check(&TokenCheckRequest {
            token: target.token.canonical(),
            sent_at: Some(ke::format_rfc3339_ms(client.now_ms())),
            pulse_version: Some(env!("CARGO_PKG_VERSION").into()),
        })
        .await;
    if let Err(e) = &check
        && let Some(code) = e.server_code()
        && !code.is_retryable()
    {
        out.step(2, 'x', "送出註冊申請");
        let pls = e.code();
        let cause = match code {
            ke::EnrollErrorCode::TokenExpired => format!(
                "Kairos 拒絕了 token {}，這個 token 已過期。本機尚未產生任何檔案，不需要清理。",
                target.token.hint()
            ),
            ke::EnrollErrorCode::TokenExhausted => format!(
                "token {} 的使用次數已滿。本機尚未產生任何檔案，不需要清理。",
                target.token.hint()
            ),
            _ => format!(
                "Kairos 拒絕了 token {}（{}）。本機尚未產生任何檔案，不需要清理。",
                target.token.hint(),
                code.as_str()
            ),
        };
        return usage_report(pls, cause, fix_for(pls, &target.server));
    }
    // ---- [2/6] agent_id ----
    let id_path = located.base.join(config.agent.id_file.trim());
    if args.reset_identity {
        let _ = std::fs::remove_file(&id_path);
        for p in [
            paths.client_key(),
            paths.client_cert(),
            paths.enrollment(),
            paths.request_key(),
        ] {
            let _ = store::remove_if_exists(&p);
        }
    }
    let (agent_id, created) = match crate::agent_id::load_or_create(&id_path) {
        Ok(x) => x,
        Err(e) => {
            return usage_report(
                codes::E_AGENT_ID_IO,
                e.to_string(),
                vec!["以系統管理員（sudo）執行。".into()],
            )
            .exit4();
        }
    };
    out.step(
        2,
        'o',
        &format!(
            "agent_id：{agent_id}（{}）",
            if created { "新產生" } else { "沿用既有" }
        ),
    );
    // ---- [3/6] 私鑰、CSR ----
    let hostname = crate::system_info::hostname();
    let host = crate::system_info::host::collect(
        args.name.as_deref().unwrap_or(&config.agent.display_name),
        target.tags.as_deref().unwrap_or(&config.agent.tags),
        Duration::from_secs(config.report.interval),
    );
    let previous_serial = existing
        .as_ref()
        .and_then(|f| f.cert_serial.clone())
        .filter(|s| ke::is_serial_hex(s));
    let n = NewEnrollment {
        agent_id: agent_id.clone(),
        server: Some(target.server.clone()),
        enroll_server: target.enroll_server.clone(),
        token: target.token.clone(),
        verification: match verified_by {
            "ca_pin" => ServerVerification::CaPin,
            "ca_file" => ServerVerification::CaFile,
            _ => ServerVerification::Tofu,
        },
        verified_by: verified_by.into(),
        ca_pin: target.pin.clone().or_else(|| trusted_pins.first().cloned()),
        trusted_pins,
        info: RequestInfo::new(&hostname, &host),
        previous_serial: previous_serial.filter(|_| registered || args.force),
    };
    let mut file = match prepare(paths, &n) {
        Ok(f) => f,
        Err(e) => {
            return usage_report(
                codes::E_NEED_ADMIN,
                e,
                vec!["以系統管理員（sudo）執行。".into()],
            )
            .exit10();
        }
    };
    file.long_poll = info
        .body
        .enroll_features
        .iter()
        .any(|f| f == ke::ENROLL_FEATURE_LONG_POLL);
    let _ = paths.save_enrollment(&file);
    // 信任錨寫成 ca.pem（之後的輪詢、撤回、過期續簽與 8443 都只信任它）
    let anchor_pem: String = match (&trust, &ca_file_certs) {
        (TrustSpec::Anchors(_), Some(c)) => c.iter().map(|c| x509::cert_pem(c)).collect(),
        _ => observed
            .anchor
            .as_ref()
            .map(|a| x509::cert_pem(a))
            .unwrap_or_default(),
    };
    if !anchor_pem.is_empty()
        && let Err(e) =
            store::write_atomic(&paths.ca(), anchor_pem.as_bytes(), FileKind::Certificate)
    {
        return usage_report(
            codes::E_NEED_ADMIN,
            format!("無法寫入 {}：{e}", paths.ca().display()),
            Vec::new(),
        )
        .exit10();
    }
    out.step(
        3,
        'o',
        &format!(
            "已產生私鑰（ECDSA P-256）：{}",
            paths.request_key().display()
        ),
    );
    let mut r = drive(
        args,
        out,
        paths,
        located,
        file,
        client,
        &agent_id,
        Some(&issues),
    )
    .await;
    if let Some(obj) = r.data.as_object_mut() {
        obj.insert("agent_id_created".into(), json!(created));
        obj.insert("server_verified_by".into(), json!(verified_by));
    }
    r
}

trait Exit4 {
    fn exit4(self) -> Self;
}

impl Exit4 for Report {
    fn exit4(mut self) -> Self {
        self.exit = 4;
        self
    }
}

/// 8444 連線階段的錯誤
async fn connect_failure(out: &Out, target: &Target, e: ApiError) -> Report {
    out.step(1, 'x', &format!("連線到註冊端點 {}", target.enroll_server));
    match &e {
        ApiError::Trust {
            code: codes::E_SERVER_CERT_VALIDITY,
            ..
        } => {
            // 本機時鐘偏差很大時的例外（ENROLL.md 第 3 節）：其他檢查照常、忽略有效期間，只取 server_time
            let trust = match &target.pin {
                Some(p) => TrustSpec::Pin(p.clone()),
                None => TrustSpec::Tofu,
            };
            let mut probe =
                EnrollClient::new(&target.enroll_server, trust, true, system_now()).ok();
            let unverified = match probe.as_mut() {
                Some(c) => c
                    .server_info()
                    .await
                    .ok()
                    .map(|r| (r.server_time, r.offset_ms)),
                None => None,
            };
            let cause = match unverified {
                Some((st, Some(off))) => {
                    crate::local_status::record(|s| {
                        s.clock_from_enroll(off, "server_info_unverified")
                    });
                    format!(
                        "註冊端點的伺服器憑證依本機時間已過期或尚未生效。本機時間 {}，Kairos 回報 {}（未驗證的參考時間），相差 {}：多半是本機時鐘不正確。",
                        crate::status_file::ts(chrono::Utc::now()),
                        st,
                        crate::status_cmd::fmt::offset_amount(off)
                    )
                }
                _ => "註冊端點的伺服器憑證已過期或尚未生效（也可能是本機時鐘不正確）。".into(),
            };
            usage_report(
                codes::E_SERVER_CERT_VALIDITY,
                cause,
                fix_for(codes::E_CLOCK_SKEW, "")[..1].to_vec(),
            )
        }
        ApiError::Trust { code, detail, .. } => usage_report(
            code,
            format!("{detail}；未送出任何資料（包括 token）。"),
            fix_for(code, ""),
        ),
        ApiError::Unsupported { status } => usage_report(
            codes::E_ENROLL_UNSUPPORTED,
            format!("註冊端點回應 HTTP {status}，不是 Kairos 0.5 的註冊 API。"),
            fix_for(codes::E_ENROLL_UNSUPPORTED, ""),
        ),
        ApiError::Network {
            refused: true,
            detail,
            ..
        } => usage_report(
            codes::E_ENROLL_UNREACHABLE,
            detail.clone(),
            fix_for(codes::E_ENROLL_UNREACHABLE, ""),
        ),
        other => {
            let code = other.code();
            let code = codes::known(code).unwrap_or(codes::W_SERVER_UNEXPECTED);
            usage_report(code, other.detail(), fix_for(code, ""))
        }
    }
}

/// 送出、等待、安裝、寫設定、確認連線
#[allow(clippy::too_many_arguments)]
async fn drive(
    args: &EnrollArgs,
    out: &Out,
    paths: &IdentityPaths,
    located: &Located,
    file: EnrollmentFile,
    client: EnrollClient,
    agent_id: &str,
    issues_in: Option<&Vec<Issue>>,
) -> Report {
    let mut issues = issues_in.cloned().unwrap_or_default();
    let tty = std::io::stderr().is_terminal();
    let wait = if args.no_wait {
        0
    } else {
        args.wait
            .unwrap_or(if tty && !args.non_interactive { 600 } else { 0 })
    };
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        });
    }
    let zone = crate::status_cmd::fmt::Zone::Local;
    let mut obs = CliObserver {
        out,
        shown_pending: false,
        zone,
    };
    let mut driver = Driver::new(paths.clone(), file, client, &mut obs);
    driver.stop_on_source_block = true;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    let outcome = driver.run(Some(deadline), &cancel).await;
    let file = driver.file.clone();
    driver.client.close().await;
    drop(driver);
    let installed_service = service_installed();
    let server = file.server.clone().unwrap_or_default();
    let mut data = json!({
        "agent_id": agent_id,
        "server": file.server,
        "enroll_server": file.enroll_server,
        "server_verified_by": file.server_verified_by,
        "enrollment": file_json(&file),
        "certificate": null,
        "config_updated": false,
        "service_restarted": false,
    });
    // 申請已交給 Kairos（或交給服務在背景重送）：寫入 pulse.toml
    let stopped = matches!(outcome, RunOutcome::Finished(Finish::Stopped { .. }));
    if !stopped {
        match write_config(located, paths, &file) {
            Ok(Some(b)) => {
                data["config_updated"] = json!(true);
                out.line(&format!("      已更新 {}（備份：{}）", located.config_path.display(), b.display()));
            }
            Ok(None) => {}
            Err(e) => issues.push(Issue::new(codes::E_CONFIG_UNREADABLE, "warning", format!("無法更新 {}：{e}", located.config_path.display()), vec!["請以系統管理員執行，或依 INSTALL 手動設定 [server] 的 client_cert、client_key、ca_cert。".into()])),
        }
    }
    match outcome {
        RunOutcome::Finished(Finish::Installed) => {
            data["certificate"] =
                json!({"serial": file.cert_serial, "not_after": file.cert_not_after});
            // 以新憑證確認連上 8443（hello），再讓服務接手
            let confirmed = confirm(located, agent_id).await;
            if let Err(e) = &confirmed {
                issues.push(
                    Issue::new(
                        codes::E_SERVER_SILENT,
                        "warning",
                        format!("已安裝憑證，但以新憑證連線到 {server} 失敗：{e}"),
                        vec!["服務會持續重試；查看 pulse status 與日誌。".into()],
                    )
                    .title("已註冊，但尚未確認連上 Kairos"),
                );
            } else if let Ok(Some(mut f)) = paths.load_enrollment() {
                f.state = state::REGISTERED.into();
                let _ = paths.save_enrollment(&f);
                data["enrollment"]["state"] = json!(state::REGISTERED);
            }
            if service_running() {
                let ok = restart_service();
                data["service_restarted"] = json!(ok);
            }
            let warn = !issues.is_empty();
            Report {
                exit: if warn { 8 } else { 0 },
                result: "registered",
                error: None,
                issues,
                data,
                summary: Some(if warn {
                    "已註冊，但有警告。".to_string()
                } else {
                    format!("已註冊並連上 Kairos（{server}）。")
                }),
            }
        }
        RunOutcome::Finished(Finish::Stopped { .. } | Finish::NotExpired) => {
            let code = match outcome {
                RunOutcome::Finished(Finish::Stopped { code }) => code,
                _ => codes::W_SERVER_UNEXPECTED,
            };
            let mut cause = file.last_error.clone().unwrap_or_default();
            if code == codes::E_REQUEST_REJECTED {
                cause = format!(
                    "管理者拒絕了申請 {}（{}，原因：{}）。本機未使用的私鑰已刪除。",
                    file.request_code.as_deref().unwrap_or("?"),
                    file.decided_by.as_deref().unwrap_or("?"),
                    file.reject_reason.as_deref().unwrap_or("未填寫")
                );
            }
            Report {
                exit: exit_for(code),
                result: "failed",
                error: Some(Issue::new(code, "error", cause, fix_for(code, &server)).details(json!({"protocol_code": file.protocol_code, "observed_ip": file.observed_ip}))),
                issues,
                data,
                summary: None,
            }
        }
        _ => {
            // 時間到、Ctrl+C：服務在背景接續
            let rc = file.request_code.clone().unwrap_or_default();
            match file.state.as_str() {
                state::PENDING_APPROVAL => {
                    out.step(
                        5,
                        'p',
                        "尚未核准。服務會在背景每 30 秒～5 分鐘查詢一次，核准後自動連線。",
                    );
                    Report {
                        exit: 7,
                        result: "pending",
                        error: None,
                        issues,
                        data,
                        summary: Some(format!("等待核准：申請編號 {rc}。查看進度：pulse status")),
                    }
                }
                state::CLOCK_REJECTED => {
                    let off = file.offset_ms.unwrap_or(0);
                    let issue = Issue::new(
                        codes::E_CLOCK_SKEW,
                        if installed_service { "warning" } else { "error" },
                        format!(
                            "本機時間 {}，Kairos 時間 {}（相差 {}，超過 5 分鐘）。Kairos 沒有建立申請，也沒有消耗 token 的使用次數；token 本身沒有問題。",
                            file.local_time.as_deref().unwrap_or("?"),
                            file.server_time.as_deref().unwrap_or("?"),
                            crate::status_cmd::fmt::offset_amount(off)
                        ),
                        if installed_service {
                            fix_for(codes::E_CLOCK_SKEW, "")
                        } else {
                            let mut f = fix_for(codes::E_CLOCK_SKEW, "")[..1].to_vec();
                            f.push("時間校正後重新執行同一指令：會從上次中斷處繼續，沿用同一把私鑰與 agent_id，不需要新的 token。這台主機沒有安裝 Pulse 服務，不會在背景自動重送。".into());
                            f
                        },
                    )
                    .title(format!("Kairos 因時鐘偏差暫不受理註冊：本機時鐘比 Kairos {} {}", if off < 0 { "慢" } else { "快" }, crate::status_cmd::fmt::offset_amount(off)))
                    .details(json!({"protocol_code": "clock_skew", "http_status": 400, "offset_ms": off, "max_skew_ms": ke::MAX_CLOCK_SKEW_MS, "local_time": file.local_time, "server_time": file.server_time, "next_submit_at": file.next_submit_at}));
                    if installed_service {
                        issues.push(issue);
                        Report { exit: 7, result: "pending", error: None, issues, data, summary: Some("註冊暫緩：Kairos 因時鐘偏差暫不受理這台主機的申請。時間校正後，Pulse 服務會立即自動重新送出。查看進度：pulse status".into()) }
                    } else {
                        Report {
                            exit: 6,
                            result: "failed",
                            error: Some(issue),
                            issues,
                            data,
                            summary: None,
                        }
                    }
                }
                _ => {
                    let code = file
                        .last_code
                        .as_deref()
                        .and_then(codes::known)
                        .unwrap_or(codes::E_ENROLL_RATE_LIMITED);
                    let issue = Issue::new(
                        code,
                        "warning",
                        format!("註冊申請尚未送出（{}）。Kairos 限制了同一個網路位址（{}）的申請速度，或註冊端點暫時無法連線；申請不會遺失。", file.last_error.clone().unwrap_or_default(), file.observed_ip.as_deref().unwrap_or("?")),
                        if installed_service { fix_for(codes::E_ENROLL_RATE_LIMITED, "") } else { vec!["稍後重新執行同一指令（會沿用同一個申請，不會重複）。".into()] },
                    )
                    .title("註冊申請暫緩送出：註冊端點忙碌中");
                    issues.push(issue);
                    Report {
                        exit: if installed_service { 7 } else { 124 },
                        result: "pending",
                        error: None,
                        issues,
                        data,
                        summary: Some("註冊申請尚未送出：註冊端點忙碌。".into()),
                    }
                }
            }
        }
    }
}

/// 以新憑證送一次 hello（確認後關閉；服務接手連線）
async fn confirm(located: &Located, agent_id: &str) -> Result<(), String> {
    let config = AppConfig::load_file(&located.config_path).map_err(|e| e.to_string())?;
    let mut server = config.server.clone();
    for p in [
        &mut server.client_cert,
        &mut server.client_key,
        &mut server.ca_cert,
    ] {
        if !p.trim().is_empty() {
            *p = located.base.join(p.trim()).display().to_string();
        }
    }
    let tls = crate::tls::build(&server)?;
    let settings = crate::network::Settings::new(
        &config,
        agent_id.to_string(),
        crate::system_info::hostname(),
    );
    let conn = crate::network::one_shot(
        &settings,
        &tls,
        crate::protocol::hello_features(false),
        None,
    )
    .await
    .map_err(|e| e.message)?;
    conn.close().await;
    Ok(())
}

/// 寫入 `pulse.toml`（設計包 §7）：`[server]` 的位址、TLS 名稱與 identity/ 的檔案，
/// 註冊端點與推導值不同時另外寫 `[enroll]`
fn write_config(
    located: &Located,
    paths: &IdentityPaths,
    file: &EnrollmentFile,
) -> std::io::Result<Option<PathBuf>> {
    let Some(server) = &file.server else {
        return Ok(None);
    };
    let host = ke::endpoint_host(server).to_string();
    let port: i64 = server
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(8443);
    let rel = |p: PathBuf| -> String {
        p.strip_prefix(&located.base)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.display().to_string())
            .replace('\\', "/")
    };
    let mut changes = vec![
        ("server", "address", T::Str(host.clone())),
        ("server", "port", T::Int(port)),
        ("server", "domain", T::Str(host.clone())),
        ("server", "ca_cert", T::Str(rel(paths.ca()))),
        ("server", "use_system_roots", T::Bool(false)),
        ("server", "client_cert", T::Str(rel(paths.client_cert()))),
        ("server", "client_key", T::Str(rel(paths.client_key()))),
    ];
    let derived = super::endpoint_of(&host, ke::DEFAULT_ENROLL_PORT).unwrap_or_default();
    if file.enroll_server != derived {
        let ehost = ke::endpoint_host(&file.enroll_server).to_string();
        let eport: i64 = file
            .enroll_server
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse().ok())
            .unwrap_or(8444);
        changes.push(("enroll", "address", T::Str(ehost)));
        changes.push(("enroll", "port", T::Int(eport)));
    }
    let backup = toml_edit::apply(&located.config_path, &changes)?;
    // 確認新的設定可以載入（失敗時還原）
    if let Err(e) = AppConfig::load_file(&located.config_path) {
        if let Some(b) = &backup {
            let _ = std::fs::copy(b, &located.config_path);
        }
        return Err(std::io::Error::other(format!(
            "寫入後的設定無法載入（已還原）：{e}"
        )));
    }
    Ok(backup.or(Some(located.config_path.clone())))
}

// ---------------------------------------------------------------------------
// pulse decommission（安裝程式完全移除時使用）
// ---------------------------------------------------------------------------

/// `pulse decommission --help`
pub const DECOMMISSION_USAGE: &str = "\
pulse decommission — 完全移除前通知 Kairos（安裝程式使用）

用法：
  pulse decommission --initiated-by <msi|install_script|package_manager|cli> [--json] [-c <pulse.toml>]

說明：
  已註冊（有用戶端憑證）時經 8443 送出除役通知（deregister）；等待核准中時經 8444 撤回申請。
  最多 15 秒，失敗時以 PLS-W3107／PLS-W3310 警告（結束代碼 8），本機檔案由安裝程式刪除。
  先停止 Pulse 服務再執行。

結束代碼：0 已通知、不需通知或略過  8 無法通知（警告）  2 用法錯誤";

/// `pulse decommission` 的參數
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecommissionArgs {
    pub initiated_by: String,
    pub json: bool,
    pub config: Option<String>,
}

pub fn parse_decommission(args: &[String]) -> Result<Option<DecommissionArgs>, UsageError> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(None);
    }
    let mut d = DecommissionArgs::default();
    let mut i = 0;
    while i < args.len() {
        let (name, inline) = match args[i].split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (args[i].clone(), None),
        };
        let mut value = || -> Result<String, UsageError> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i).cloned().ok_or_else(|| {
                UsageError::new(format!("PLS-E0002：{name} 需要值"), DECOMMISSION_USAGE)
            })
        };
        match name.as_str() {
            "--initiated-by" => {
                let v = value()?;
                if !matches!(
                    v.as_str(),
                    "msi" | "install_script" | "package_manager" | "cli"
                ) {
                    return Err(UsageError::new(
                        format!("PLS-E0002：--initiated-by 不支援「{v}」"),
                        DECOMMISSION_USAGE,
                    ));
                }
                d.initiated_by = v;
            }
            "-c" | "--config" => d.config = Some(value()?),
            "--json" => d.json = true,
            other => {
                return Err(UsageError::new(
                    format!("PLS-E0001：pulse decommission 未知的參數「{other}」"),
                    DECOMMISSION_USAGE,
                ));
            }
        }
        i += 1;
    }
    if d.initiated_by.is_empty() {
        return Err(UsageError::new(
            "PLS-E0004：需要 --initiated-by".into(),
            DECOMMISSION_USAGE,
        ));
    }
    Ok(Some(d))
}

/// 執行 `pulse decommission`
pub fn run_decommission(args: &DecommissionArgs) -> u8 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("錯誤：{e}");
            return 1;
        }
    };
    rt.block_on(async {
        let located = match locate(args.config.as_deref()) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("錯誤 PLS-E0101：{e}");
                return 3;
            }
        };
        let paths = super::identity_paths(&located.config, &located.base);
        let file = paths.load_enrollment().ok().flatten();
        let initiated = match args.initiated_by.as_str() {
            "msi" => InitiatedBy::Msi,
            "install_script" => InitiatedBy::InstallScript,
            "package_manager" => InitiatedBy::PackageManager,
            _ => InitiatedBy::Cli,
        };
        let usable = {
            let c = located.config.server.client_cert.trim();
            !c.is_empty() && located.base.join(c).is_file()
        };
        let (key, value, warn) = if usable {
            let d = crate::decommission::deregister(&located.config, &located.base, &args.initiated_by).await;
            let warn = d.result == "unreported";
            ("deregistration", d.to_json(), warn)
        } else if let Some(f) = file.filter(|f| f.state == state::PENDING_APPROVAL) {
            let w = match super::service::trust_for(&paths, &f, Some((&located.config, &located.base)))
                .and_then(|t| EnrollClient::new(&f.enroll_server, t, false, system_now()).ok())
            {
                Some(mut c) => flow::withdraw(&paths, &f, &mut c, WithdrawReason::UninstallPurge, initiated).await,
                None => flow::Withdrawal { result: "unreported", code: Some(codes::W_WITHDRAW_UNREPORTED), reason: Some("unreachable"), cause_code: Some(codes::E_CA_PIN), protocol_code: None, request_code: f.request_code.clone(), request_state: None, attempts: 0, already_withdrawn: false },
            };
            if w.result == "unreported" {
                log::warn!("{}request_code={} reason={}", codes::tag(codes::INSTALL_WITHDRAWAL_UNREPORTED, Some(codes::W_WITHDRAW_UNREPORTED)), w.request_code.as_deref().unwrap_or("-"), w.reason.unwrap_or("-"));
            }
            ("withdrawal", withdrawal_json(&w), w.result == "unreported")
        } else {
            ("deregistration", json!({"result": "skipped", "code": null, "reason": null}), false)
        };
        let doc = json!({"schema": "pulse.decommission/v1", "ok": true, "exit_code": if warn { 8 } else { 0 }, "data": {key: value}});
        if args.json {
            print!("{}", crate::status_cmd::json::to_ascii_json(&doc));
        } else {
            println!("{key}: {}", value["result"].as_str().unwrap_or("?"));
            if warn {
                eprintln!(
                    "警告 {}：無法通知 Kairos（reason={}）；請 Kairos 管理者在主控台處理這台主機",
                    value["code"].as_str().unwrap_or(""),
                    value["reason"].as_str().unwrap_or("?")
                );
            }
        }
        if warn { 8 } else { 0 }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Option<EnrollArgs>, UsageError> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_flags_and_rejects_conflicts() {
        let a = p(&[
            "--server",
            "kairos.example.gov.tw",
            "--token=KRT1-GE8Q-MMGW-WG5P-DP97",
            "--tag",
            "a:b",
            "--tag",
            "c:d",
            "--no-wait",
            "-y",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(a.server.as_deref(), Some("kairos.example.gov.tw"));
        assert_eq!(a.tags, ["a:b", "c:d"]);
        assert!(a.no_wait && a.yes);
        assert!(p(&["--help"]).unwrap().is_none());
        assert!(p(&["--bogus"]).unwrap_err().message.contains("PLS-E0001"));
        assert!(
            p(&["--wait", "5", "--no-wait"])
                .unwrap_err()
                .message
                .contains("PLS-E0003")
        );
        assert!(
            p(&["--ca-pin", "x", "--ca-file", "y"])
                .unwrap_err()
                .message
                .contains("PLS-E0003")
        );
        assert!(
            p(&["--wait", "abc"])
                .unwrap_err()
                .message
                .contains("PLS-E0002")
        );
        assert!(p(&["--token"]).unwrap_err().message.contains("PLS-E0002"));
    }

    #[test]
    fn endpoints_default_to_8443_and_8444() {
        assert_eq!(
            normalize_server("Kairos.Example.gov.tw", 8443).unwrap(),
            "kairos.example.gov.tw:8443"
        );
        assert_eq!(
            normalize_server("10.20.0.15:9443", 8443).unwrap(),
            "10.20.0.15:9443"
        );
        assert_eq!(
            normalize_server("2001:db8::1", 8443).unwrap(),
            "[2001:db8::1]:8443"
        );
        assert_eq!(
            resolve_enroll_server(None, "kairos.example.gov.tw:8443").unwrap(),
            "kairos.example.gov.tw:8444"
        );
        assert_eq!(
            resolve_enroll_server(Some(":9444"), "kairos.example.gov.tw:8443").unwrap(),
            "kairos.example.gov.tw:9444"
        );
        assert_eq!(
            resolve_enroll_server(Some("enroll.example.gov.tw"), "kairos.example.gov.tw:8443")
                .unwrap(),
            "enroll.example.gov.tw:8444"
        );
        assert_eq!(
            resolve_enroll_server(None, "[2001:db8::1]:8443").unwrap(),
            "[2001:db8::1]:8444"
        );
    }

    #[test]
    fn install_string_and_flags_are_merged_and_checked() {
        let v = crate::enroll::vectors::Vectors::load();
        let args = EnrollArgs {
            install_string: Some(v.get("install_string_full").into()),
            ..Default::default()
        };
        let t = resolve_target(&args).unwrap_or_else(|_| panic!("應可解析"));
        assert_eq!(t.server, "kairos.example.gov.tw:8443");
        assert_eq!(t.enroll_server, "enroll.example.gov.tw:9444");
        assert_eq!(t.pin.as_deref(), Some(v.get("ca_pin")));
        assert_eq!(
            t.tags.as_deref(),
            Some(&["site:tainan".to_string(), "line:5".to_string()][..])
        );
        // 只有 s 時註冊端點為 s 的主機加 8444
        let args = EnrollArgs {
            install_string: Some(v.get("install_string").into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_target(&args).ok().unwrap().enroll_server,
            "kairos.example.gov.tw:8444"
        );
        // 與旗標不同：PLS-E0003
        let args = EnrollArgs {
            install_string: Some(v.get("install_string").into()),
            server: Some("other.example:8443".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_target(&args).err().unwrap().error.unwrap().code,
            "PLS-E0003"
        );
        // 打錯 token：在本機攔下（PLS-E3001，結束代碼 2）
        let args = EnrollArgs {
            server: Some("k.example".into()),
            token: Some("KRT1-GE8Q-MMGW-WG5P-DP98".into()),
            ..Default::default()
        };
        let r = resolve_target(&args).err().unwrap();
        assert_eq!((r.error.unwrap().code, r.exit), ("PLS-E3001", 2));
        // 缺少 server
        let args = EnrollArgs {
            token: Some(v.get("token").into()),
            ..Default::default()
        };
        assert_eq!(resolve_target(&args).err().unwrap().exit, 2);
        // 壞掉的安裝字串
        let args = EnrollArgs {
            install_string: Some("pulse-enroll:v2:xxx".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_target(&args).err().unwrap().error.unwrap().code,
            "PLS-E0002"
        );
    }

    #[test]
    fn exit_codes_follow_cli_spec() {
        assert_eq!(exit_for("PLS-E3003"), 6);
        assert_eq!(exit_for("PLS-E2008"), 6);
        assert_eq!(exit_for("PLS-E3005"), 6);
        assert_eq!(exit_for("PLS-E1406"), 5);
        assert_eq!(exit_for("PLS-E2007"), 5);
        assert_eq!(exit_for("PLS-E3001"), 2);
        assert_eq!(exit_for("PLS-E3301"), 11);
        assert_eq!(exit_for("PLS-E1107"), 4);
    }

    fn quiet_out() -> Out {
        Out {
            style: Style {
                unicode: false,
                color: false,
            },
            json: true,
            quiet: true,
            total: 6,
        }
    }

    fn temp_config(server_port: u16) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("pulse-enroll-cli-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("pulse.toml");
        std::fs::write(
            &p,
            format!(
                "# 測試
[server]
address = \"127.0.0.1\"
port = {server_port}

[log]
level = \"info\"
"
            ),
        )
        .unwrap();
        p
    }

    #[tokio::test]
    async fn enroll_writes_config_then_refuses_a_second_enrollment() {
        use crate::enroll::fake::{FakeKairos, Options};
        let fake = FakeKairos::start(Options::default()).await;
        let token = "KRT1-GE8Q-MMGW-WG5P-DP97";
        fake.lock()
            .add_token(token, ke::TokenApproval::Auto, "valid");
        let cfg = temp_config(1);
        let base = cfg.parent().unwrap().to_path_buf();
        let args = EnrollArgs {
            server: Some("127.0.0.1:1".into()),
            enroll_server: Some(fake.endpoint.clone()),
            token: Some(token.into()),
            ca_pin: Some(fake.ca_pin.clone()),
            non_interactive: true,
            json: true,
            config: Some(cfg.display().to_string()),
            ..Default::default()
        };
        let r = run_async(&args, &quiet_out()).await;
        // 8443（127.0.0.1:1）連不上：已註冊但尚未確認連上，結束代碼 8
        assert_eq!((r.result, r.exit), ("registered", 8), "{:?}", r.error);
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.starts_with("# 測試"), "保留註解：{text}");
        assert!(
            text.contains("client_cert = \"identity/client.pem\""),
            "{text}"
        );
        assert!(text.contains("use_system_roots = false"), "{text}");
        assert!(
            text.contains("[enroll]"),
            "註冊端點與推導值不同時寫 [enroll]：{text}"
        );
        let config = AppConfig::load_file(&cfg).unwrap();
        assert_eq!(config.server.domain, "127.0.0.1");
        assert!(base.join("identity/client.pem").is_file());
        assert!(std::fs::read_dir(&base).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("pulse.toml.bak-")
        }));
        // 再執行一次（沒有 --force）：PLS-E3301，結束代碼 11
        let r = run_async(&args, &quiet_out()).await;
        assert_eq!((r.error.unwrap().code, r.exit), ("PLS-E3301", 11));
    }

    #[tokio::test]
    async fn pending_request_can_be_cancelled_from_the_cli() {
        use crate::enroll::fake::{FakeKairos, Options};
        let fake = FakeKairos::start(Options::default()).await;
        let token = "KRT1-GE8Q-MMGW-WG5P-DP97";
        fake.lock()
            .add_token(token, ke::TokenApproval::Manual, "valid");
        let cfg = temp_config(1);
        let base = cfg.parent().unwrap().to_path_buf();
        let args = EnrollArgs {
            server: Some("127.0.0.1:1".into()),
            enroll_server: Some(fake.endpoint.clone()),
            token: Some(token.into()),
            ca_pin: Some(fake.ca_pin.clone()),
            no_wait: true,
            non_interactive: true,
            json: true,
            config: Some(cfg.display().to_string()),
            ..Default::default()
        };
        let r = run_async(&args, &quiet_out()).await;
        assert_eq!((r.result, r.exit), ("pending", 7));
        // 進行中：新的 token 不加 --force 時 PLS-E3305
        let r = run_async(&args, &quiet_out()).await;
        assert_eq!((r.error.unwrap().code, r.exit), ("PLS-E3305", 11));
        let cancel = EnrollArgs {
            cancel: true,
            yes: true,
            json: true,
            config: Some(cfg.display().to_string()),
            ..Default::default()
        };
        let r = run_async(&cancel, &quiet_out()).await;
        assert_eq!(
            (r.result, r.exit),
            ("cancelled", 0),
            "{:?}",
            r.issues.first().map(|i| &i.cause)
        );
        assert_eq!(r.data["withdrawal"]["result"], "ok");
        assert_eq!(fake.lock().requests[0].state, ke::RequestState::Withdrawn);
        assert!(!base.join("identity/enrollment.json").exists());
        assert!(!base.join("identity/request.key").exists());
        assert!(base.join("agent.id").exists(), "agent_id 保留");
    }

    #[tokio::test]
    async fn token_rejected_by_token_check_leaves_no_files() {
        use crate::enroll::fake::{FakeKairos, Options};
        let fake = FakeKairos::start(Options::default()).await;
        let token = "KRT1-GE8Q-MMGW-WG5P-DP97";
        fake.lock()
            .add_token(token, ke::TokenApproval::Auto, "expired");
        let cfg = temp_config(1);
        let base = cfg.parent().unwrap().to_path_buf();
        let args = EnrollArgs {
            server: Some("127.0.0.1:1".into()),
            enroll_server: Some(fake.endpoint.clone()),
            token: Some(token.into()),
            ca_pin: Some(fake.ca_pin.clone()),
            non_interactive: true,
            json: true,
            config: Some(cfg.display().to_string()),
            ..Default::default()
        };
        let r = run_async(&args, &quiet_out()).await;
        assert_eq!((r.error.unwrap().code, r.exit), ("PLS-E3003", 6));
        assert!(!base.join("identity/request.key").exists());
        assert_eq!(fake.lock().hits("requests"), 0, "不送出申請");
        // 非互動、沒有指紋：PLS-E1406，結束代碼 5，不連線
        let args = EnrollArgs {
            ca_pin: None,
            ..args
        };
        let r = run_async(&args, &quiet_out()).await;
        assert_eq!((r.error.unwrap().code, r.exit), ("PLS-E1406", 5));
    }

    #[test]
    fn decommission_args() {
        let a =
            |v: &[&str]| parse_decommission(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            a(&["--initiated-by", "msi", "--json"])
                .unwrap()
                .unwrap()
                .initiated_by,
            "msi"
        );
        assert!(a(&["--json"]).is_err());
        assert!(a(&["--initiated-by", "someone"]).is_err());
    }
}
