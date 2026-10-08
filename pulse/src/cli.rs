//! 命令列參數解析（自行解析 `std::env::args`，不引入 clap）。

/// 使用說明（`-h` / `--help`）。
pub const USAGE: &str = "\
用法：pulse [選項]

不帶選項時啟動監控代理，連線到設定中的 Kairos 伺服器。

選項：
  --print-agent-id   印出本機 agent_id（agent.id 不存在時先產生）後結束，不連線
  -h, --help         顯示此說明

設定來源（後者覆蓋前者）：內建預設值 → 目前目錄的 pulse.toml → PULSE__<SECTION>__<KEY> 環境變數";

/// 要執行的動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 正常啟動監控代理
    Run,
    /// 印出 agent_id 後結束
    PrintAgentId,
    /// 顯示使用說明
    Help,
}

/// 解析命令列參數（不含程式名稱）。
///
/// `-h` / `--help` 優先於其他選項；未知參數回傳錯誤訊息。
pub fn parse_args<I, S>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut command = Command::Run;
    for arg in args {
        match arg.as_ref() {
            "-h" | "--help" => return Ok(Command::Help),
            "--print-agent-id" => command = Command::PrintAgentId,
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
    fn unknown_args_rejected() {
        for bad in ["--bogus", "-x", "print-agent-id", "--print-agent-id=1", ""] {
            let err = parse_args([bad]).unwrap_err();
            assert!(err.contains("未知的參數"), "{bad:?} → {err}");
        }
        assert!(parse_args(["--print-agent-id", "--bogus"]).is_err());
    }
}
