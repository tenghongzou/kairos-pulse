//! 命令列參數解析（自行解析 `std::env::args`，不引入 clap）。

/// 完整版本字串：`pulse <CARGO_PKG_VERSION> (<commit>)`，用於 `--version` 與啟動日誌。
///
/// commit 由 `build.rs` 決定（環境變數 `BUILD_COMMIT` → `git rev-parse --short HEAD` → `unknown`）。
/// hello 的 `agent_version` 欄位不使用這個字串，仍只送 `CARGO_PKG_VERSION`。
pub const VERSION: &str = concat!(
    "pulse ",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("PULSE_BUILD_COMMIT"),
    ")"
);

/// 使用說明（`-h` / `--help`）。
pub const USAGE: &str = "\
用法：pulse [命令] [選項]

不帶命令時啟動監控代理，連線到設定中的 Kairos 伺服器。

命令：
  status             顯示服務、連線、身分與回報狀態（讀取本機狀態檔，不需要連線；pulse status --help）
  enroll             以註冊 token 向 Kairos 註冊這台主機（pulse enroll --help）
  decommission       完全移除前通知 Kairos（安裝程式使用；pulse decommission --help）
  explain <代碼>     說明錯誤代碼，例如 pulse explain PLS-E2003

選項：
  --print-agent-id   印出本機 agent_id（agent.id 不存在時先產生）後結束，不連線
  -V, --version      顯示版本與建置的 commit 後結束（不讀取設定）
  -h, --help         顯示此說明

設定來源（後者覆蓋前者）：內建預設值 → 目前目錄的 pulse.toml → PULSE__<SECTION>__<KEY> 環境變數";

/// `pulse status --help`（`cli-spec.md` §3.2 的共同結構）
pub const STATUS_USAGE: &str = "\
pulse status — 顯示服務、連線、身分與回報狀態

用法：
  pulse status [--brief | --json] [--watch [秒]] [--exit-codes=nagios] [-c <pulse.toml>]

說明：
  讀取執行中的 Pulse 每 10 秒寫出的本機狀態檔（預設為工作目錄下的 logs/status.json），
  並向 systemd／Windows 服務控制管理員查詢服務狀態。不連線、不影響服務。
  設定檔依序找：--config、目前目錄的 pulse.toml、pulse 執行檔所在目錄的 pulse.toml。

選項：
      --brief              單行輸出（第一欄為本機狀態），給監控系統或登入訊息使用
      --json               輸出機器可讀的 JSON（純 ASCII，schema pulse.status/v1）
      --watch [秒]         每 N 秒（預設 2）重新顯示；按 Ctrl+C 結束
      --exit-codes=nagios  使用 Nagios 慣例的結束代碼（0 OK、1 WARNING、2 CRITICAL、3 UNKNOWN）並輸出單行
  -c, --config <路徑>      pulse.toml 的位置（狀態檔等相對路徑以它所在的目錄解析）
      --instance <N>       多實例 N（讀取 /var/lib/pulse/N/status.json 與 pulse-N.service）
      --color <模式>       auto（預設）、always、never；也會遵守 NO_COLOR
      --no-color           同 --color never
      --ascii              不使用 Unicode 符號（也可設定 PULSE_SYMBOLS=ascii）
  -q, --quiet              不顯示警告等級的說明（錯誤仍會顯示）
  -v, --verbose            說明中加上技術細節
  -h, --help               顯示此說明

範例：
  pulse status
  pulse status --brief
  pulse status --json

結束代碼：
  0 線上  7 待核准  8 有警告  9 嚴重、離線或未知（服務未執行、狀態檔過時）
  2 用法錯誤  10 權限不足（無法讀取狀態檔）";

/// `--color`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

/// `pulse status` 的參數
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusArgs {
    pub brief: bool,
    pub json: bool,
    /// `--watch [秒]`
    pub watch: Option<u64>,
    /// `--exit-codes=nagios`
    pub nagios: bool,
    pub config: Option<String>,
    /// `--instance <N>`：多實例 N（狀態檔 `/var/lib/pulse/N/status.json`）
    pub instance: Option<u32>,
    pub color: Color,
    pub ascii: bool,
    pub quiet: bool,
    pub verbose: bool,
}

/// 要執行的動作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 正常啟動監控代理
    Run,
    /// 印出 agent_id 後結束
    PrintAgentId,
    /// 印出版本後結束
    Version,
    /// 顯示使用說明
    Help,
    /// `pulse status`
    Status(StatusArgs),
    /// `pulse status --help`
    StatusHelp,
    /// `pulse explain <代碼>`
    Explain(String),
    /// `pulse enroll`
    Enroll(Box<crate::enroll::cli::EnrollArgs>),
    /// `pulse enroll --help`
    EnrollHelp,
    /// `pulse decommission`
    Decommission(crate::enroll::cli::DecommissionArgs),
    /// `pulse decommission --help`
    DecommissionHelp,
}

/// 參數錯誤：訊息與要附上的用法
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    pub message: String,
    pub usage: &'static str,
}

impl UsageError {
    pub fn new(message: String, usage: &'static str) -> Self {
        Self { message, usage }
    }
}

/// 解析命令列參數（不含程式名稱）。
///
/// `-h` / `--help` 優先於其他選項，其次是 `-V` / `--version`；未知參數回傳錯誤訊息。
/// 第一個參數為 `status`、`explain`、`run` 時為子命令。
pub fn parse_args<I, S>(args: I) -> Result<Command, UsageError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_string()).collect();
    match args.first().map(String::as_str) {
        Some("status") => return parse_status(&args[1..]),
        Some("explain") => {
            return match &args[1..] {
                [h] if h == "-h" || h == "--help" => Ok(Command::Help),
                [code] if !code.starts_with('-') => Ok(Command::Explain(code.clone())),
                _ => Err(UsageError::new(
                    "PLS-E0002：用法為 pulse explain <代碼>，例如 pulse explain PLS-E2003"
                        .to_string(),
                    USAGE,
                )),
            };
        }
        Some("run") => return parse_global(&args[1..]),
        Some("enroll") => {
            return Ok(match crate::enroll::cli::parse(&args[1..])? {
                Some(a) => Command::Enroll(Box::new(a)),
                None => Command::EnrollHelp,
            });
        }
        Some("decommission") => {
            return Ok(match crate::enroll::cli::parse_decommission(&args[1..])? {
                Some(a) => Command::Decommission(a),
                None => Command::DecommissionHelp,
            });
        }
        _ => {}
    }
    parse_global(&args)
}

fn parse_global(args: &[String]) -> Result<Command, UsageError> {
    let mut command = Command::Run;
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => command = Command::Version,
            "--print-agent-id" => {
                if command != Command::Version {
                    command = Command::PrintAgentId;
                }
            }
            other => {
                return Err(UsageError::new(
                    format!("PLS-E0001：未知的參數「{other}」（使用 --help 查看用法）"),
                    USAGE,
                ));
            }
        }
    }
    Ok(command)
}

fn parse_status(args: &[String]) -> Result<Command, UsageError> {
    let err = |m: String| Err(UsageError::new(m, STATUS_USAGE));
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Command::StatusHelp);
    }
    let mut s = StatusArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg, None),
        };
        // 需要值的選項：--x=值 或 --x 值
        let mut value = |what: &str| -> Result<String, UsageError> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i).cloned().ok_or_else(|| {
                UsageError::new(format!("PLS-E0002：{name} 需要{what}"), STATUS_USAGE)
            })
        };
        match name {
            "--brief" if inline.is_none() => s.brief = true,
            "--json" if inline.is_none() => s.json = true,
            "--watch" => {
                let secs = match inline.clone() {
                    Some(v) => Some(v),
                    // 秒數可省略：下一個參數是數字時才當成秒數
                    None => match args.get(i + 1) {
                        Some(next)
                            if next.chars().all(|c| c.is_ascii_digit()) && !next.is_empty() =>
                        {
                            i += 1;
                            Some(next.clone())
                        }
                        _ => None,
                    },
                };
                let secs = match secs {
                    None => 2,
                    Some(v) => match v.parse::<u64>() {
                        Ok(n @ 1..=3600) => n,
                        _ => {
                            return err(format!(
                                "PLS-E0002：--watch 的秒數必須是 1～3600 的整數（目前為「{v}」）"
                            ));
                        }
                    },
                };
                s.watch = Some(secs);
            }
            "--exit-codes" => match value("模式（nagios）")?.as_str() {
                "nagios" => s.nagios = true,
                other => {
                    return err(format!(
                        "PLS-E0002：--exit-codes 只支援 nagios（目前為「{other}」）"
                    ));
                }
            },
            "-c" | "--config" => s.config = Some(value("設定檔路徑")?),
            "--instance" => {
                let v = value("實例編號")?;
                match v.parse::<u32>() {
                    Ok(n @ 1..=999) => s.instance = Some(n),
                    _ => {
                        return err(format!(
                            "PLS-E0002：--instance 必須是 1～999 的整數（目前為「{v}」）"
                        ));
                    }
                }
            }
            "--color" => {
                s.color = match value("模式（auto、always、never）")?.as_str() {
                    "auto" => Color::Auto,
                    "always" => Color::Always,
                    "never" => Color::Never,
                    other => {
                        return err(format!(
                            "PLS-E0002：--color 只接受 auto、always、never（目前為「{other}」）"
                        ));
                    }
                }
            }
            "--no-color" if inline.is_none() => s.color = Color::Never,
            "--ascii" if inline.is_none() => s.ascii = true,
            "-q" | "--quiet" if inline.is_none() => s.quiet = true,
            "-v" | "--verbose" if inline.is_none() => s.verbose = true,
            "--all" => {
                return err(
                    "PLS-E0002：--all（列出本機所有實例）尚未支援；請在各實例的目錄執行 pulse status，\
                     或以 --config 指定該實例的 pulse.toml"
                        .to_string(),
                );
            }
            other => {
                return err(format!(
                    "PLS-E0001：pulse status 未知的參數「{other}」（使用 pulse status --help 查看用法）"
                ));
            }
        }
        i += 1;
    }
    if s.brief && s.json {
        return err("PLS-E0003：--brief 與 --json 不能同時使用".to_string());
    }
    if s.watch.is_some() && s.json {
        return err(
            "PLS-E0003：--watch 與 --json 不能同時使用（--json 只輸出一份文件）".to_string(),
        );
    }
    Ok(Command::Status(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(args: &[&str]) -> String {
        parse_args(args.iter().copied()).unwrap_err().message
    }

    #[test]
    fn no_args_runs() {
        assert_eq!(parse_args(Vec::<String>::new()), Ok(Command::Run));
        assert_eq!(parse_args(["run"]), Ok(Command::Run));
    }

    #[test]
    fn print_agent_id() {
        assert_eq!(parse_args(["--print-agent-id"]), Ok(Command::PrintAgentId));
        // 重複指定不算錯
        assert_eq!(
            parse_args(["--print-agent-id", "--print-agent-id"]),
            Ok(Command::PrintAgentId)
        );
    }

    #[test]
    fn help_wins() {
        assert_eq!(parse_args(["-h"]), Ok(Command::Help));
        assert_eq!(parse_args(["--help"]), Ok(Command::Help));
        assert_eq!(parse_args(["--print-agent-id", "-h"]), Ok(Command::Help));
        // --help 在未知參數之前時直接顯示說明
        assert_eq!(parse_args(["--help", "--bogus"]), Ok(Command::Help));
    }

    #[test]
    fn version_flag() {
        assert_eq!(parse_args(["--version"]), Ok(Command::Version));
        assert_eq!(parse_args(["-V"]), Ok(Command::Version));
        // 與 --print-agent-id 同時指定時，不論順序都是 --version
        assert_eq!(
            parse_args(["--version", "--print-agent-id"]),
            Ok(Command::Version)
        );
        assert_eq!(parse_args(["--print-agent-id", "-V"]), Ok(Command::Version));
        // --help 仍優先
        assert_eq!(parse_args(["-V", "--help"]), Ok(Command::Help));
        // 小寫 -v 與帶值的寫法不接受
        assert!(parse_args(["-v"]).is_err());
        assert!(parse_args(["--version=1"]).is_err());
    }

    #[test]
    fn version_string_format() {
        // D3：pulse <版本> (<commit>)，單行
        let expected_prefix = format!("pulse {} (", env!("CARGO_PKG_VERSION"));
        assert!(VERSION.starts_with(&expected_prefix), "{VERSION}");
        assert!(VERSION.ends_with(')'), "{VERSION}");
        let commit = &VERSION[expected_prefix.len()..VERSION.len() - 1];
        assert!(!commit.is_empty(), "{VERSION}");
        assert!(
            commit
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+')),
            "{VERSION}"
        );
        assert!(!VERSION.contains('\n'));
        assert!(USAGE.contains("--version") && USAGE.contains("-V"));
    }

    #[test]
    fn unknown_args_rejected() {
        for bad in ["--bogus", "-x", "print-agent-id", "--print-agent-id=1", ""] {
            let e = err(&[bad]);
            assert!(
                e.contains("未知的參數") && e.contains("PLS-E0001"),
                "{bad:?} → {e}"
            );
        }
        assert!(parse_args(["--print-agent-id", "--bogus"]).is_err());
        assert_eq!(parse_args(["--bogus"]).unwrap_err().usage, USAGE);
    }

    #[test]
    fn status_arguments() {
        let status = |args: &[&str]| match parse_args(args.iter().copied()).unwrap() {
            Command::Status(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(status(&["status"]), StatusArgs::default());
        assert!(status(&["status", "--brief"]).brief);
        assert!(status(&["status", "--json"]).json);
        assert_eq!(status(&["status", "--watch"]).watch, Some(2));
        assert_eq!(status(&["status", "--watch", "5"]).watch, Some(5));
        assert_eq!(status(&["status", "--watch=7", "--brief"]).watch, Some(7));
        assert_eq!(
            status(&["status", "--watch", "--brief"]),
            StatusArgs {
                watch: Some(2),
                brief: true,
                ..StatusArgs::default()
            }
        );
        assert!(status(&["status", "--exit-codes=nagios"]).nagios);
        assert!(status(&["status", "--exit-codes", "nagios"]).nagios);
        assert_eq!(
            status(&["status", "-c", "/opt/pulse/pulse.toml"])
                .config
                .as_deref(),
            Some("/opt/pulse/pulse.toml")
        );
        assert_eq!(
            status(&["status", "--config=x.toml"]).config.as_deref(),
            Some("x.toml")
        );
        assert_eq!(status(&["status", "--instance", "2"]).instance, Some(2));
        assert!(err(&["status", "--instance", "0"]).contains("PLS-E0002"));
        assert_eq!(status(&["status", "--no-color"]).color, Color::Never);
        assert_eq!(
            status(&["status", "--color", "always"]).color,
            Color::Always
        );
        let s = status(&["status", "--ascii", "-q", "-v"]);
        assert!(s.ascii && s.quiet && s.verbose);
        assert_eq!(parse_args(["status", "--help"]), Ok(Command::StatusHelp));
        assert_eq!(
            parse_args(["status", "--bogus", "-h"]),
            Ok(Command::StatusHelp)
        );
    }

    #[test]
    fn status_usage_errors() {
        assert!(err(&["status", "--bogus"]).contains("PLS-E0001"));
        assert!(err(&["status", "--brief", "--json"]).contains("PLS-E0003"));
        assert!(err(&["status", "--watch", "--json"]).contains("PLS-E0003"));
        assert!(err(&["status", "--watch=0"]).contains("PLS-E0002"));
        assert!(err(&["status", "--exit-codes=icinga"]).contains("PLS-E0002"));
        assert!(err(&["status", "--color", "rainbow"]).contains("PLS-E0002"));
        assert!(err(&["status", "-c"]).contains("PLS-E0002"));
        assert!(err(&["status", "--all"]).contains("尚未支援"));
        assert_eq!(
            parse_args(["status", "--bogus"]).unwrap_err().usage,
            STATUS_USAGE
        );
    }

    #[test]
    fn explain_arguments() {
        assert_eq!(
            parse_args(["explain", "PLS-E2003"]),
            Ok(Command::Explain("PLS-E2003".into()))
        );
        assert!(parse_args(["explain"]).is_err());
        assert!(parse_args(["explain", "a", "b"]).is_err());
    }
}
