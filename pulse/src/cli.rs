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
用法：pulse [選項]

不帶選項時啟動監控代理，連線到設定中的 Kairos 伺服器。

選項：
  --print-agent-id   印出本機 agent_id（agent.id 不存在時先產生）後結束，不連線
  -V, --version      顯示版本與建置的 commit 後結束（不讀取設定）
  -h, --help         顯示此說明

設定來源（後者覆蓋前者）：內建預設值 → 目前目錄的 pulse.toml → PULSE__<SECTION>__<KEY> 環境變數";

/// 要執行的動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 正常啟動監控代理
    Run,
    /// 印出 agent_id 後結束
    PrintAgentId,
    /// 印出版本後結束
    Version,
    /// 顯示使用說明
    Help,
}

/// 解析命令列參數（不含程式名稱）。
///
/// `-h` / `--help` 優先於其他選項，其次是 `-V` / `--version`；未知參數回傳錯誤訊息。
pub fn parse_args<I, S>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut command = Command::Run;
    for arg in args {
        match arg.as_ref() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => command = Command::Version,
            "--print-agent-id" => {
                if command != Command::Version {
                    command = Command::PrintAgentId;
                }
            }
            other => return Err(format!("未知的參數「{other}」（使用 --help 查看用法）")),
        }
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_runs() {
        assert_eq!(parse_args(Vec::<String>::new()), Ok(Command::Run));
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
            let err = parse_args([bad]).unwrap_err();
            assert!(err.contains("未知的參數"), "{bad:?} → {err}");
        }
        assert!(parse_args(["--print-agent-id", "--bogus"]).is_err());
    }
}
