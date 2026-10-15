//! Windows 服務（商業審計 O4）：`pulse service install|uninstall|run`。
//!
//! - `install`：向服務控制管理員註冊 `Pulse`（自動啟動、LocalSystem、失敗時 10 秒後重新啟動，
//!   一天內最多 3 次）。執行檔路徑為目前的 `pulse.exe`，服務參數為 `service run --workdir <目錄>`。
//!   MSI 安裝時不使用這個命令：服務由 MSI 的 `ServiceInstall` 表註冊（同樣的參數，見
//!   `pulse/packaging/windows/pulse.wxs`）。
//! - `uninstall`：停止（最多等 15 秒）並刪除服務。
//! - `run`：服務進入點，由服務控制管理員啟動，不要手動執行。先把目前目錄切換到 `--workdir`
//!   （預設為 `pulse.exe` 所在目錄），再以與 `pulse`／`pulse run` 相同的流程執行：`pulse.toml`、
//!   `agent.id`、`identity\`、`buffer\` 與日誌 `logs\pulse-YYYY-MM-DD.log` 都在這個目錄下，
//!   不會落在 `C:\Windows\System32`（服務的預設目前目錄）。本機狀態檔仍是
//!   `%ProgramData%\Pulse\status.json`（`pulse status` 讀取）。
//!
//! 服務收到「停止」或「關機」時以與 Ctrl+C 相同的方式結束（送出 TLS close_notify，最多約 1.5 秒）；
//! 結束代碼以服務特定的結束代碼回報（`sc.exe query Pulse` 的 `SERVICE_EXIT_CODE`：
//! 1 一般錯誤、3 設定錯誤、4 憑證錯誤，與 Linux 相同）。
//!
//! 只能在 Windows 執行；其他平台以用法錯誤（2）結束。Windows 事件記錄尚未實作（`codes.rs` 的
//! `win_event_id` 已定案）。

use std::path::PathBuf;

/// 服務名稱（`pulse status` 在 Windows 上查詢的名稱，`status_cmd::service::default_unit`）
#[cfg_attr(not(windows), allow(dead_code))]
pub const SERVICE_NAME: &str = "Pulse";
/// 服務的顯示名稱與描述
#[cfg_attr(not(windows), allow(dead_code))]
pub const DISPLAY_NAME: &str = "Pulse monitoring agent";
#[cfg_attr(not(windows), allow(dead_code))]
pub const DESCRIPTION: &str =
    "Pulse：把本機的系統資訊回報給 Kairos 監控伺服器（mTLS）。設定、身分與日誌在安裝目錄下。";
/// Windows 服務的安裝、移除、啟動失敗（需要系統管理員權限、服務已存在或不存在）〔暫定〕
#[cfg_attr(not(windows), allow(dead_code))]
pub const E_SERVICE_INSTALL: &str = "PLS-E8205";
/// 服務進入點無法執行（不是由服務控制管理員啟動、工作目錄不存在）〔暫定〕
#[cfg_attr(not(windows), allow(dead_code))]
pub const E_SERVICE_RUN: &str = "PLS-E8206";

/// `pulse service --help`
pub const USAGE: &str = "\
pulse service — Windows 服務

用法：
  pulse service install [--workdir <目錄>] [--start]
  pulse service uninstall
  pulse service run [--workdir <目錄>]     （服務進入點，由服務控制管理員啟動；不要手動執行）

說明：
  install 以系統管理員權限執行：註冊服務 Pulse（自動啟動、LocalSystem、失敗時 10 秒後重新啟動，
  一天內最多 3 次），服務參數為 service run --workdir <目錄>。--workdir 預設為 pulse.exe 所在的目錄；
  pulse.toml、agent.id、identity\\、buffer\\ 與 logs\\ 都在這個目錄下。--start 註冊後立即啟動。
  以 MSI 安裝時服務已由 MSI 註冊，不需要執行 install。
  uninstall 停止並刪除服務（不刪除任何檔案；完全移除前先執行 pulse decommission）。

結束代碼：0 完成、1 服務控制管理員回報錯誤（PLS-E8205）、2 用法錯誤或不是 Windows";

/// `pulse service` 的動作
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceArgs {
    Install {
        workdir: Option<PathBuf>,
        start: bool,
    },
    Uninstall,
    Run {
        workdir: Option<PathBuf>,
    },
}

/// 解析 `pulse service` 之後的參數；`--help` 時回傳 `None`
pub fn parse(args: &[String]) -> Result<Option<ServiceArgs>, crate::cli::UsageError> {
    let err = |m: String| crate::cli::UsageError::new(m, USAGE);
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(None);
    }
    let Some(action) = args.first() else {
        return Err(err(
            "PLS-E0002：需要動作：install、uninstall 或 run（pulse service --help）".into(),
        ));
    };
    let mut workdir = None;
    let mut start = false;
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (a, None),
        };
        match name {
            "--workdir" if action != "uninstall" => {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or_else(|| err("PLS-E0002：--workdir 需要目錄".into()))?
                    }
                };
                if v.trim().is_empty() {
                    return Err(err("PLS-E0002：--workdir 不可為空".into()));
                }
                workdir = Some(PathBuf::from(v));
            }
            "--start" if action == "install" && inline.is_none() => start = true,
            other => {
                return Err(err(format!(
                    "PLS-E0001：pulse service {action} 未知的參數「{other}」（pulse service --help）"
                )));
            }
        }
        i += 1;
    }
    match action.as_str() {
        "install" => Ok(Some(ServiceArgs::Install { workdir, start })),
        "uninstall" => Ok(Some(ServiceArgs::Uninstall)),
        "run" => Ok(Some(ServiceArgs::Run { workdir })),
        other => Err(err(format!(
            "PLS-E0002：未知的動作「{other}」：install、uninstall 或 run"
        ))),
    }
}

/// 服務的工作目錄：`--workdir`（相對路徑以目前目錄解析），預設為執行檔所在目錄
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn resolve_workdir(
    workdir: Option<&std::path::Path>,
    cwd: &std::path::Path,
    exe: &std::path::Path,
) -> PathBuf {
    match workdir {
        Some(w) => cwd.join(w),
        None => exe
            .parent()
            .map_or_else(|| cwd.to_path_buf(), std::path::Path::to_path_buf),
    }
}

/// 執行 `pulse service …`，回傳結束代碼
pub fn run(args: &ServiceArgs) -> u8 {
    imp::run(args)
}

#[cfg(not(windows))]
mod imp {
    pub fn run(_: &super::ServiceArgs) -> u8 {
        eprintln!(
            "錯誤 PLS-E0002：pulse service 只用於 Windows；Linux 請使用 systemd unit（pulse.service，INSTALL.md 第 7 節）"
        );
        crate::EXIT_USAGE
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::OsString;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;
    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    fn fail(what: &str, e: impl std::fmt::Display) -> u8 {
        eprintln!("錯誤 {E_SERVICE_INSTALL}：{what}：{e}");
        if e.to_string().contains("5)") || e.to_string().to_lowercase().contains("access") {
            eprintln!("  處理方式：以系統管理員身分開啟命令提示字元或 PowerShell 後再執行。");
        }
        crate::EXIT_GENERAL
    }

    pub fn run(args: &ServiceArgs) -> u8 {
        match args {
            ServiceArgs::Install { workdir, start } => install(workdir.as_deref(), *start),
            ServiceArgs::Uninstall => uninstall(),
            ServiceArgs::Run { workdir } => run_service(workdir.as_deref()),
        }
    }

    fn install(workdir: Option<&std::path::Path>, start: bool) -> u8 {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return fail("無法取得 pulse.exe 的路徑", e),
        };
        let cwd = std::env::current_dir().unwrap_or_default();
        let dir = resolve_workdir(workdir, &cwd, &exe);
        if !dir.is_dir() {
            eprintln!(
                "錯誤 {E_SERVICE_INSTALL}：工作目錄 {} 不存在",
                dir.display()
            );
            return crate::EXIT_USAGE;
        }
        let manager = match ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        ) {
            Ok(m) => m,
            Err(e) => return fail("無法連線到服務控制管理員", e),
        };
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe.clone(),
            launch_arguments: vec![
                OsString::from("service"),
                OsString::from("run"),
                OsString::from("--workdir"),
                dir.clone().into_os_string(),
            ],
            dependencies: Vec::new(),
            account_name: None,
            account_password: None,
        };
        let service = match manager.create_service(
            &info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        ) {
            Ok(s) => s,
            Err(e) => {
                return fail(
                    "無法註冊服務 Pulse（已註冊時先執行 pulse service uninstall）",
                    e,
                );
            }
        };
        let _ = service.set_description(DESCRIPTION);
        // 失敗（含以非 0 結束代碼停止）時 10 秒後重新啟動；一天內最多 3 次，避免設定錯誤時無限重試
        let restart = ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(10),
        };
        let actions = ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![restart.clone(), restart.clone(), restart]),
        };
        if let Err(e) = service
            .update_failure_actions(actions)
            .and_then(|_| service.set_failure_actions_on_non_crash_failures(true))
        {
            eprintln!("警告：無法設定失敗時的重新啟動：{e}");
        }
        println!(
            "已註冊服務 {SERVICE_NAME}（自動啟動）：{} service run --workdir {}",
            exe.display(),
            dir.display()
        );
        if start {
            if let Err(e) = service.start::<&str>(&[]) {
                return fail("無法啟動服務 Pulse", e);
            }
            println!("已啟動服務 {SERVICE_NAME}；以 pulse status 確認連線狀態");
        } else {
            println!("啟動：sc.exe start {SERVICE_NAME}（或 pulse enroll 完成後自動重新啟動）");
        }
        0
    }

    fn uninstall() -> u8 {
        let manager =
            match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
                Ok(m) => m,
                Err(e) => return fail("無法連線到服務控制管理員", e),
            };
        let service = match manager.open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        ) {
            Ok(s) => s,
            Err(e) => return fail("找不到服務 Pulse", e),
        };
        if let Ok(status) = service.query_status()
            && status.current_state != ServiceState::Stopped
        {
            let _ = service.stop();
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                match service.query_status() {
                    Ok(s) if s.current_state == ServiceState::Stopped => break,
                    _ => std::thread::sleep(Duration::from_millis(500)),
                }
            }
        }
        if let Err(e) = service.delete() {
            return fail("無法刪除服務 Pulse", e);
        }
        println!("已移除服務 {SERVICE_NAME}（安裝目錄中的設定、身分與日誌保留不動）");
        0
    }

    /// 服務的停止要求（控制處理常式 → 監控代理）
    static STOP: OnceLock<CancellationToken> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    fn run_service(workdir: Option<&std::path::Path>) -> u8 {
        let exe = std::env::current_exe().unwrap_or_default();
        let cwd = std::env::current_dir().unwrap_or_default();
        let dir = resolve_workdir(workdir, &cwd, &exe);
        // 服務的目前目錄預設是 System32：先切換，pulse.toml 與相對路徑才會以安裝目錄解析
        if let Err(e) = std::env::set_current_dir(&dir) {
            eprintln!(
                "錯誤 {E_SERVICE_RUN}：無法切換到工作目錄 {}：{e}",
                dir.display()
            );
            return crate::EXIT_GENERAL;
        }
        let _ = STOP.set(CancellationToken::new());
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!(
                    "錯誤 {E_SERVICE_RUN}：pulse service run 只能由服務控制管理員啟動（{e}）。\
                     在主控台執行時改用 pulse（不帶參數）"
                );
                crate::EXIT_USAGE
            }
        }
    }

    fn status(state: ServiceState, exit: u8, wait: Duration) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: if exit == 0 {
                ServiceExitCode::NO_ERROR
            } else {
                ServiceExitCode::ServiceSpecific(u32::from(exit))
            },
            checkpoint: 0,
            wait_hint: wait,
            process_id: None,
        }
    }

    fn service_main(_arguments: Vec<OsString>) {
        let stop = STOP.get().cloned().unwrap_or_default();
        let handler_stop = stop.clone();
        let handle =
            match service_control_handler::register(SERVICE_NAME, move |control| match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    handler_stop.cancel();
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }) {
                Ok(h) => h,
                Err(_) => return,
            };
        let _ = handle.set_service_status(status(ServiceState::Running, 0, Duration::ZERO));
        let code = crate::run_agent(false, Some(stop));
        let _ = handle.set_service_status(status(ServiceState::Stopped, code, Duration::ZERO));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Option<ServiceArgs>, crate::cli::UsageError> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn service_arguments_are_parsed() {
        assert_eq!(p(&["--help"]).unwrap(), None);
        assert_eq!(
            p(&["install", "--start", "--workdir", "C:\\Pulse"]).unwrap(),
            Some(ServiceArgs::Install {
                workdir: Some(PathBuf::from("C:\\Pulse")),
                start: true
            })
        );
        assert_eq!(
            p(&["run", "--workdir=D:\\agent"]).unwrap(),
            Some(ServiceArgs::Run {
                workdir: Some(PathBuf::from("D:\\agent"))
            })
        );
        assert_eq!(p(&["uninstall"]).unwrap(), Some(ServiceArgs::Uninstall));
        for bad in [
            &[][..],
            &["start"][..],
            &["uninstall", "--workdir", "x"][..],
            &["run", "--start"][..],
            &["install", "--workdir"][..],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
        // pulse service 經由主命令解析
        assert!(matches!(
            crate::cli::parse_args(["service", "install"]).unwrap(),
            crate::cli::Command::Service(ServiceArgs::Install { .. })
        ));
        assert_eq!(
            crate::cli::parse_args(["service", "--help"]).unwrap(),
            crate::cli::Command::ServiceHelp
        );
    }

    #[test]
    fn workdir_defaults_to_the_executable_directory_not_system32() {
        let exe = std::path::Path::new("/opt/pulse/pulse");
        let cwd = std::path::Path::new("/windows/system32");
        assert_eq!(resolve_workdir(None, cwd, exe), PathBuf::from("/opt/pulse"));
        assert_eq!(
            resolve_workdir(Some(std::path::Path::new("/data/pulse")), cwd, exe),
            PathBuf::from("/data/pulse")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn other_platforms_refuse_with_a_usage_error() {
        assert_eq!(run(&ServiceArgs::Uninstall), crate::EXIT_USAGE);
    }
}
