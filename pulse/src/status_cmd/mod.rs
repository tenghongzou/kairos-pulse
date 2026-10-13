//! `pulse status`：讀取本機狀態檔與服務管理員，顯示服務、連線、回報、身分、設定與時鐘
//! （設計包 `pulse/cli-spec.md` §4）。不需要 root、不連線、不影響服務。
//!
//! - 設定檔：`--config` → 目前目錄的 `pulse.toml` → `pulse` 執行檔所在目錄的 `pulse.toml`；
//!   狀態檔（`status.file`，預設 `logs/status.json`）等相對路徑以設定檔所在的目錄解析。
//!   設定檔有誤時仍以預設的狀態檔位置繼續，並在「設定」列顯示錯誤。
//! - 結束代碼：0 線上、7 待核准、8 有警告、9 嚴重／離線／未知、2 用法錯誤、10 無法讀取狀態檔
//!   （權限不足）；`--exit-codes=nagios` 時為 0／1／2／3。

pub mod eval;
pub mod fmt;
pub mod issues;
pub mod json;
pub mod render;
pub mod service;
pub mod term;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::cli::{Color, StatusArgs};
use crate::codes;
use crate::config_manager::{AppConfig, LOCAL_CONFIG_FILE};
use crate::status_file::{self as sf, ReadError};
use eval::{ConfigView, FileState, Input};
use term::{ColorChoice, Stream, Style};

/// 用法錯誤
const EXIT_USAGE: u8 = 2;
/// 權限不足（`cli-style.md` §8）
const EXIT_PERMISSION: u8 = 10;

/// 執行 `pulse status`；回傳結束代碼
pub fn run(args: &StatusArgs) -> u8 {
    let Some(interval) = args.watch else {
        let out = once(args);
        emit(&out);
        return out.exit_code;
    };
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    loop {
        let out = once(args);
        if tty {
            // 重新整理整個畫面（清除畫面並移到左上角）；不支援 ANSI 的舊主控台改為逐次附加
            let style = term::detect(Stream::Stdout, color_choice(args.color), args.ascii, false);
            if style.color {
                print!("\x1b[2J\x1b[H");
            } else {
                println!();
            }
        } else {
            println!();
        }
        emit(&out);
        let next = chrono::Local::now().format("%H:%M:%S");
        println!("（每 {interval} 秒更新，上次 {next}；按 Ctrl+C 結束）");
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

/// 一次執行的輸出
#[derive(Debug, Default)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
}

fn emit(out: &Output) {
    // 寫入失敗（例如管線已關閉）時不 panic
    let _ = std::io::stdout().write_all(out.stdout.as_bytes());
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(out.stderr.as_bytes());
}

fn color_choice(c: Color) -> ColorChoice {
    match c {
        Color::Auto => ColorChoice::Auto,
        Color::Always => ColorChoice::Always,
        Color::Never => ColorChoice::Never,
    }
}

/// 設定檔與解析相對路徑的目錄
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// 找到的設定檔（不存在時為 `None`）
    pub config: Option<PathBuf>,
    /// 相對路徑以這個目錄解析
    pub base: PathBuf,
}

/// 找設定檔：`--config` → 目前目錄 → 執行檔所在目錄；都沒有時以目前目錄為準
/// （但目前目錄沒有狀態檔、執行檔所在目錄有時，改用執行檔所在目錄）
pub fn locate(explicit: Option<&Path>, cwd: &Path, exe_dir: Option<&Path>) -> Located {
    if let Some(p) = explicit {
        let p = cwd.join(p);
        let base = p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map_or_else(|| cwd.to_path_buf(), Path::to_path_buf);
        return Located {
            config: Some(p),
            base,
        };
    }
    for dir in std::iter::once(cwd).chain(exe_dir) {
        let p = dir.join(LOCAL_CONFIG_FILE);
        if p.is_file() {
            return Located {
                config: Some(p),
                base: dir.to_path_buf(),
            };
        }
    }
    let default_status = AppConfig::defaults().status.file;
    let base = match exe_dir {
        Some(exe) if !cwd.join(&default_status).exists() && exe.join(&default_status).exists() => {
            exe.to_path_buf()
        }
        _ => cwd.to_path_buf(),
    };
    Located { config: None, base }
}

/// 設定檔錯誤的代碼
fn config_error_code(e: &config::ConfigError) -> &'static str {
    match e {
        config::ConfigError::Message(_) => codes::E_CONFIG_RANGE,
        _ => codes::E_CONFIG_SYNTAX,
    }
}

/// 單行、截到 400 字元的錯誤文字
fn one_line(s: &str) -> String {
    let s = s.replace(['\r', '\n'], " ");
    s.chars().take(400).collect()
}

/// 執行一次（不輸出）
pub fn once(args: &StatusArgs) -> Output {
    let now = Utc::now();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let located = locate(
        args.config.as_deref().map(Path::new),
        &cwd,
        exe_dir.as_deref(),
    );
    let out_style = term::detect(
        Stream::Stdout,
        color_choice(args.color),
        args.ascii,
        args.json,
    );
    let err_style = term::detect(
        Stream::Stderr,
        color_choice(args.color),
        args.ascii,
        args.json,
    );

    // 設定
    let mut view = ConfigView {
        file: located
            .config
            .as_ref()
            .filter(|p| p.is_file())
            .map(|p| p.display().to_string()),
        expected: located.base.join(LOCAL_CONFIG_FILE).display().to_string(),
        error: None,
    };
    if let Some(p) = &located.config
        && !p.is_file()
    {
        let msg = format!(
            "{}：指定的設定檔不存在（{}）",
            codes::E_BAD_ARG_VALUE,
            p.display()
        );
        return Output {
            stdout: String::new(),
            stderr: format!("錯誤：{msg}\n\n{}\n", crate::cli::STATUS_USAGE),
            exit_code: EXIT_USAGE,
        };
    }
    let config = match &located.config {
        // 沒有讀取權限：不算設定有誤（以服務回報的設定為準），狀態檔等用預設位置
        Some(p) if std::fs::File::open(p).is_err() => AppConfig::defaults(),
        Some(p) => match AppConfig::load_file(p) {
            Ok(c) => c,
            Err(e) => {
                view.error = Some((config_error_code(&e), one_line(&e.to_string())));
                AppConfig::defaults()
            }
        },
        None => AppConfig::defaults(),
    };
    let status_path = located.base.join(config.status.file.trim());
    let status_text = status_path.display().to_string();

    // 狀態檔
    let file = match sf::read(&status_path) {
        Ok(f) => FileState::Ok(Box::new(f)),
        Err(ReadError::Missing) => FileState::Missing,
        Err(ReadError::Invalid(e)) => FileState::Invalid(one_line(&e)),
        Err(ReadError::Io(e)) => FileState::Invalid(one_line(&e.to_string())),
        Err(ReadError::PermissionDenied(e)) => {
            return permission_denied(args, &status_text, &e, err_style, now);
        }
    };
    let file_ref = match &file {
        FileState::Ok(f) => Some(f.as_ref()),
        _ => None,
    };

    // 服務管理員與行程
    let unit_hint = file_ref.and_then(|f| f.service.unit.clone());
    let service = service::query(unit_hint.as_deref());
    let pid_alive = file_ref
        .filter(|f| f.running)
        .and_then(|f| service::process_alive(f.pid));
    let agent_id = std::fs::read_to_string(located.base.join(config.agent.id_file.trim()))
        .ok()
        .map(|s| s.trim().trim_start_matches('\u{feff}').to_ascii_lowercase())
        .filter(|s| uuid::Uuid::parse_str(s).is_ok());
    let input = Input {
        file: file.clone(),
        status_path: status_text.clone(),
        service: service.clone(),
        pid_alive,
        config: view.clone(),
        now,
        now_mono: sf::mono_ms(),
        zone: fmt::Zone::Local,
        windows: cfg!(windows),
        hostname: sysinfo::System::host_name().unwrap_or_else(|| "（未知）".to_string()),
        agent_id,
        version: env!("CARGO_PKG_VERSION").to_string(),
        commit: env!("PULSE_BUILD_COMMIT").to_string(),
    };
    let report = eval::evaluate(&input);
    let exit_code = if args.nagios {
        report.status.nagios_code()
    } else {
        report.status.exit_code()
    };

    if args.json {
        let manager = service
            .as_ref()
            .filter(|s| s.state != service::SvcState::NotInstalled)
            .map(|s| s.manager);
        let unit = service.as_ref().map(|s| s.unit.as_str());
        return Output {
            stdout: json::document(&json::JsonInput {
                report: &report,
                file: &file,
                status_path: &status_text,
                service_manager: manager,
                service_unit: unit,
                config_file: view.file.clone(),
                exit_code,
                generated_at: now,
            }),
            stderr: String::new(),
            exit_code,
        };
    }
    if args.brief || args.nagios {
        return Output {
            stdout: format!("{}\n", report.brief),
            stderr: String::new(),
            exit_code,
        };
    }
    Output {
        stdout: render::rows(&report, &out_style),
        stderr: render::issues(&report, &err_style, args.quiet, args.verbose),
        exit_code,
    }
}

/// 沒有權限讀取狀態檔：命令本身無法完成（結束代碼 10）
fn permission_denied(
    args: &StatusArgs,
    path: &str,
    e: &std::io::Error,
    style: Style,
    now: chrono::DateTime<Utc>,
) -> Output {
    let fix = if cfg!(windows) {
        "以系統管理員身分執行，或請管理者讓這個帳號可以讀取狀態檔所在的目錄。".to_string()
    } else {
        "以 root（sudo pulse status）或服務帳號群組（pulse）的成員執行；或請管理者把這個帳號加入該群組。"
            .to_string()
    };
    let issue = issues::Issue {
        code: codes::E_NEED_ADMIN.to_string(),
        severity: issues::Severity::Error,
        title: "無法讀取狀態檔：權限不足".to_string(),
        cause: vec![format!(
            "沒有權限讀取 {path}（{}）。狀態檔與日誌目錄只有服務帳號與其群組可讀。",
            one_line(&e.to_string())
        )],
        fix_intro: Some(fix),
        fix: Vec::new(),
        details: [("path".to_string(), serde_json::json!(path))]
            .into_iter()
            .collect(),
    };
    if args.json {
        return Output {
            stdout: json::failure(&issue, EXIT_PERMISSION, now),
            stderr: String::new(),
            exit_code: EXIT_PERMISSION,
        };
    }
    if args.brief || args.nagios {
        return Output {
            stdout: format!(
                "{:<9} permission_denied {}\n",
                "unknown",
                codes::E_NEED_ADMIN
            ),
            stderr: String::new(),
            exit_code: if args.nagios { 3 } else { EXIT_PERMISSION },
        };
    }
    Output {
        stdout: String::new(),
        stderr: render::issue_block(&issue, &style, args.verbose),
        exit_code: EXIT_PERMISSION,
    }
}

/// `pulse explain <代碼>`：印出代碼的標題（完整說明在 ERROR-CODES.md）；代碼不存在時結束代碼 2
pub fn explain(code: &str) -> u8 {
    let code = code.trim().to_ascii_uppercase();
    match codes::title(&code) {
        Some(title) => {
            println!("{code}：{title}");
            println!(
                "完整說明與對照表：Pulse 發佈包中的 ERROR-CODES.md（README.md「錯誤代碼」）。"
            );
            0
        }
        None => {
            eprintln!(
                "錯誤：{}：不認得的錯誤代碼「{code}」（格式為 PLS-E0000，例如 pulse explain PLS-E2003）",
                codes::E_UNKNOWN_ARG
            );
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod golden;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pulse-status-cmd-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_is_located_in_order() {
        let cwd = temp_dir("cwd");
        let exe = temp_dir("exe");
        // 都沒有：目前目錄
        assert_eq!(
            locate(None, &cwd, Some(&exe)),
            Located {
                config: None,
                base: cwd.clone()
            }
        );
        // 執行檔所在目錄有狀態檔
        std::fs::create_dir_all(exe.join("logs")).unwrap();
        std::fs::write(exe.join("logs").join("status.json"), "{}").unwrap();
        assert_eq!(locate(None, &cwd, Some(&exe)).base, exe);
        // 執行檔所在目錄有 pulse.toml
        std::fs::write(exe.join("pulse.toml"), "").unwrap();
        assert_eq!(
            locate(None, &cwd, Some(&exe)).config,
            Some(exe.join("pulse.toml"))
        );
        // 目前目錄優先
        std::fs::write(cwd.join("pulse.toml"), "").unwrap();
        assert_eq!(locate(None, &cwd, Some(&exe)).base, cwd);
        // --config 最優先；相對路徑以目前目錄解析
        let other = temp_dir("other");
        let explicit = other.join("x.toml");
        let l = locate(Some(&explicit), &cwd, Some(&exe));
        assert_eq!((l.config, l.base), (Some(explicit.clone()), other.clone()));
        for d in [cwd, exe, other] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn config_errors_are_classified() {
        assert_eq!(
            config_error_code(&config::ConfigError::Message("x".into())),
            codes::E_CONFIG_RANGE
        );
        assert_eq!(
            config_error_code(&config::ConfigError::NotFound("x".into())),
            codes::E_CONFIG_SYNTAX
        );
    }

    #[test]
    fn status_in_a_directory_without_pulse_is_not_installed() {
        // 空目錄、沒有服務（測試環境不一定有 systemd unit）：嚴重（9）或用法正確
        let dir = temp_dir("empty");
        let args = StatusArgs {
            config: Some(dir.join("pulse.toml").display().to_string()),
            ..StatusArgs::default()
        };
        // 指定的設定檔不存在 → 用法錯誤
        assert_eq!(once(&args).exit_code, EXIT_USAGE);
        std::fs::write(dir.join("pulse.toml"), "").unwrap();
        let out = once(&StatusArgs {
            json: true,
            ..args.clone()
        });
        assert_eq!(out.exit_code, 9, "{}", out.stdout);
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["schema"], "pulse.status/v1");
        assert_eq!(v["data"]["status"], "critical");
        assert_eq!(v["exit_code"], 9);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_status_file_exits_10() {
        use std::os::unix::fs::PermissionsExt as _;
        // root 不受檔案權限限制，略過
        if std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(1) == Some("0"))
        {
            return;
        }
        let dir = temp_dir("perm");
        std::fs::write(dir.join("pulse.toml"), "").unwrap();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let status = dir.join("logs").join("status.json");
        std::fs::write(&status, "{}").unwrap();
        std::fs::set_permissions(&status, std::fs::Permissions::from_mode(0o000)).unwrap();
        let args = StatusArgs {
            config: Some(dir.join("pulse.toml").display().to_string()),
            ..StatusArgs::default()
        };
        let out = once(&args);
        assert_eq!(out.exit_code, 10);
        assert!(out.stderr.contains("PLS-E8203"), "{}", out.stderr);
        let out = once(&StatusArgs { json: true, ..args });
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["exit_code"], 10);
        assert_eq!(v["error"]["code"], "PLS-E8203");
        assert!(v["data"].is_null());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explain_knows_the_codes() {
        assert_eq!(explain("PLS-W4202"), 0);
        assert_eq!(explain("pls-e2003"), 0);
        assert_eq!(explain("PLS-E9999"), EXIT_USAGE);
    }
}
