//! Windows 服務（商業審計 O4）：`pulse service install|uninstall|run`。
//!
//! - `install`：向服務控制管理員註冊 `Pulse`（自動啟動、LocalSystem、失敗復原見下方）。執行檔路徑為目前的
//!   `pulse.exe`，服務參數為 `service run --workdir <目錄>`。
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
//! # 失敗復原（[`recovery`]；ADR-0001 第 1.2 節 X1～X3、第 8 節 U13）
//!
//! 服務異常結束（含以非 0 結束代碼停止）時由服務控制管理員重新啟動：第 1 次失敗後等 10 秒、第 2 次 60 秒、
//! 第 3 次起每 300 秒，**沒有次數上限**（服務控制管理員在失敗次數超過動作數之後重複最後一個動作）；連續
//! 一天沒有失敗才把失敗計數歸零。0.8.0～0.10.1 設定的是三個「10 秒後重新啟動」，文件寫成「一天內最多
//! 3 次」是錯的：實際上每 10 秒重新啟動、沒有上限（X1）。
//!
//! 服務每次啟動時比對完整的設定（動作陣列、重設期、「非 0 結束代碼也算失敗」旗標），不一致就改回上述內容
//! （X2：先前只在「沒有任何動作」時補上，MSI 註冊的服務以外——舊版留下的設定、被部分修改過的設定——都不會
//! 被修正）。管理者或群組原則另外設定的失敗動作因此會在下次啟動時被改回來。該旗標的變更依 Microsoft 的
//! 文件要到下次開機才生效（X3）：由 MSI 註冊的服務在第一次啟動時才設定它，重開機前非 0 結束代碼可能不會
//! 觸發重新啟動（待實機驗證，`docs/windows-vm-validation.md`）。
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
/// Windows 服務的安裝、移除、啟動失敗（需要系統管理員權限、服務已存在或不存在）
#[cfg_attr(not(windows), allow(unused_imports))]
pub use crate::codes::{E_SERVICE_INSTALL, E_SERVICE_RUN};

/// 服務的失敗復原政策：改這裡就同時改變 `pulse service install` 與服務啟動時的校正
/// （[`desired_recovery`]）。ADR-0001 第 8 節 U13 的 (a)「頻率有上限、持續重試」在沒有啟動器時的等效設定；
/// 現行服務是否採用、設定或憑證錯誤（結束代碼 3／4）要不要不重新啟動，使用者尚未決定
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub mod recovery {
    /// 第 N 次失敗後等待多久再重新啟動（秒）；失敗次數超過陣列長度後，服務控制管理員重複最後一個
    pub const RESTART_DELAYS_SECS: &[u64] = &[10, 60, 300];
    /// 連續這麼久（秒）沒有失敗才把失敗計數歸零
    pub const RESET_PERIOD_SECS: u64 = 86_400;
    /// 以非 0 結束代碼正常停止（回報了 `SERVICE_STOPPED`）是否也算失敗。`true`：結束代碼 1／3／4 都會
    /// 依上面的延遲重新啟動（0.8.0 起的行為；設定錯誤修好後不必手動啟動，代價是修好之前每 300 秒重試一次）。
    /// `false`：只有行程異常終止才重新啟動，任何非 0 結束代碼都停在「已停止」——服務控制管理員無法只排除
    /// 結束代碼 3／4，要做到與 Linux 的 unit 相同（3／4 不重啟、其他重啟）需要啟動器（ADR-0001 第 4.4 節）
    pub const ON_NON_CRASH_FAILURE: bool = true;
}

/// 失敗動作的種類（`SC_ACTION_TYPE`）
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    None,
    Restart,
    Reboot,
    RunCommand,
}

/// 服務的失敗復原設定中由 Pulse 管理的部分（重開機訊息與命令列不在其中：只用重新啟動，不比對也不修改）
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryConfig {
    /// 依失敗次數排列的動作與延遲（毫秒）
    pub actions: Vec<(RecoveryAction, u64)>,
    /// 失敗計數的重設期（秒）；`None` 為永不重設
    pub reset_secs: Option<u64>,
    /// 「非 0 結束代碼也算失敗」旗標
    pub on_non_crash_failure: bool,
}

/// [`recovery`] 對應的設定
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn desired_recovery() -> RecoveryConfig {
    RecoveryConfig {
        actions: recovery::RESTART_DELAYS_SECS
            .iter()
            .map(|secs| (RecoveryAction::Restart, secs * 1000))
            .collect(),
        reset_secs: Some(recovery::RESET_PERIOD_SECS),
        on_non_crash_failure: recovery::ON_NON_CRASH_FAILURE,
    }
}

/// 目前的設定與期望的設定不同之處（給日誌用的說明）；完全相同時為空。`observed` 為 `None` 表示讀不到
/// 目前的設定（視為不一致）
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn recovery_drift(observed: Option<&RecoveryConfig>, desired: &RecoveryConfig) -> Vec<String> {
    let Some(observed) = observed else {
        return vec!["讀不到目前的設定".to_string()];
    };
    let mut out = Vec::new();
    if observed.actions != desired.actions {
        out.push(if observed.actions.is_empty() {
            "沒有失敗動作".to_string()
        } else {
            format!(
                "失敗動作為 {}",
                observed
                    .actions
                    .iter()
                    .map(|(kind, ms)| format!("{kind:?}/{ms}ms"))
                    .collect::<Vec<_>>()
                    .join("、")
            )
        });
    }
    if observed.reset_secs != desired.reset_secs {
        out.push(match observed.reset_secs {
            Some(secs) => format!("重設期為 {secs} 秒"),
            None => "重設期為永不".to_string(),
        });
    }
    if observed.on_non_crash_failure != desired.on_non_crash_failure {
        out.push(format!(
            "「非 0 結束代碼也算失敗」為{}",
            if observed.on_non_crash_failure {
                "開啟"
            } else {
                "關閉"
            }
        ));
    }
    out
}

/// 服務啟動時校正失敗復原設定的結果（日誌等級與訊息）。校正發生在日誌初始化之前，由 `run_agent` 在日誌
/// 可用後取出並記錄；沒有校正（設定已一致、不是以服務執行）時為 `None`
pub fn take_startup_note() -> Option<(log::Level, String)> {
    imp::take_startup_note()
}

/// `pulse service --help`
pub const USAGE: &str = "\
pulse service — Windows 服務

用法：
  pulse service install [--workdir <目錄>] [--start]
  pulse service uninstall
  pulse service run [--workdir <目錄>]     （服務進入點，由服務控制管理員啟動；不要手動執行）

說明：
  install 以系統管理員權限執行：註冊服務 Pulse（自動啟動、LocalSystem），服務參數為
  service run --workdir <目錄>。--workdir 預設為 pulse.exe 所在的目錄；
  pulse.toml、agent.id、identity\\、buffer\\ 與 logs\\ 都在這個目錄下。--start 註冊後立即啟動。
  失敗復原：服務異常結束（含以非 0 結束代碼停止）時由服務控制管理員重新啟動，第 1 次失敗後等 10 秒、
  第 2 次 60 秒、第 3 次起每 300 秒，沒有次數上限；連續一天沒有失敗才重新從 10 秒算起。服務每次啟動時
  會把這項設定校正回來（含由 MSI 註冊的服務；以 sc.exe qfailure Pulse 查看）。
  以 MSI 安裝時服務已由 MSI 註冊，不需要執行 install。
  uninstall 停止並刪除服務（不刪除任何檔案；完全移除前先執行 pulse decommission）。

結束代碼：0 完成、1 服務控制管理員回報錯誤（PLS-E8105）、2 用法錯誤或不是 Windows";

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
    pub fn take_startup_note() -> Option<(log::Level, String)> {
        None
    }

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
        // 失敗復原（模組說明；`recovery`）
        if let Err(e) = set_failure_actions(&service, &desired_recovery()) {
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

    fn to_action_type(kind: RecoveryAction) -> ServiceActionType {
        match kind {
            RecoveryAction::None => ServiceActionType::None,
            RecoveryAction::Restart => ServiceActionType::Restart,
            RecoveryAction::Reboot => ServiceActionType::Reboot,
            RecoveryAction::RunCommand => ServiceActionType::RunCommand,
        }
    }

    fn from_action_type(kind: ServiceActionType) -> RecoveryAction {
        match kind {
            ServiceActionType::None => RecoveryAction::None,
            ServiceActionType::Restart => RecoveryAction::Restart,
            ServiceActionType::Reboot => RecoveryAction::Reboot,
            ServiceActionType::RunCommand => RecoveryAction::RunCommand,
        }
    }

    /// 把失敗復原設為 `config`：動作陣列與重設期、「非 0 結束代碼也算失敗」旗標。重開機訊息與命令列
    /// 不變（`None` 表示不修改）。動作含重新啟動時，服務控制管理員要求控制代碼有 `SERVICE_START` 權限
    fn set_failure_actions(
        service: &windows_service::service::Service,
        config: &RecoveryConfig,
    ) -> windows_service::Result<()> {
        service.update_failure_actions(ServiceFailureActions {
            reset_period: match config.reset_secs {
                Some(secs) => ServiceFailureResetPeriod::After(Duration::from_secs(secs)),
                None => ServiceFailureResetPeriod::Never,
            },
            reboot_msg: None,
            command: None,
            actions: Some(
                config
                    .actions
                    .iter()
                    .map(|&(kind, ms)| ServiceAction {
                        action_type: to_action_type(kind),
                        delay: Duration::from_millis(ms),
                    })
                    .collect(),
            ),
        })?;
        service.set_failure_actions_on_non_crash_failures(config.on_non_crash_failure)
    }

    /// 讀出目前的失敗復原設定；任何一部分讀不到時為 `None`
    fn observe_recovery(service: &windows_service::service::Service) -> Option<RecoveryConfig> {
        let current = service.get_failure_actions().ok()?;
        let flag = service.get_failure_actions_on_non_crash_failures().ok()?;
        Some(RecoveryConfig {
            actions: current
                .actions
                .unwrap_or_default()
                .iter()
                .map(|a| {
                    (
                        from_action_type(a.action_type),
                        u64::try_from(a.delay.as_millis()).unwrap_or(u64::MAX),
                    )
                })
                .collect(),
            reset_secs: match current.reset_period {
                ServiceFailureResetPeriod::Never => None,
                ServiceFailureResetPeriod::After(d) => Some(d.as_secs()),
            },
            on_non_crash_failure: flag,
        })
    }

    /// 服務啟動時校正失敗復原設定的結果（[`take_startup_note`]）
    static STARTUP_NOTE: std::sync::Mutex<Option<(log::Level, String)>> =
        std::sync::Mutex::new(None);

    pub fn take_startup_note() -> Option<(log::Level, String)> {
        STARTUP_NOTE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// 服務啟動時：比對完整的失敗復原設定（動作陣列、重設期、「非 0 結束代碼也算失敗」旗標），與
    /// [`desired_recovery`] 不一致就改回來。由 MSI 的 ServiceInstall 註冊的服務沒有失敗動作（wixl 不支援
    /// MsiServiceConfigFailureActions）、0.8.0～0.10.1 註冊的服務是三個 10 秒，都在這裡校正
    fn ensure_failure_actions() {
        let note = |level, message: String| {
            *STARTUP_NOTE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((level, message));
        };
        let service = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .and_then(|manager| {
                manager.open_service(
                    SERVICE_NAME,
                    ServiceAccess::QUERY_CONFIG
                        | ServiceAccess::CHANGE_CONFIG
                        | ServiceAccess::START,
                )
            });
        let service = match service {
            Ok(s) => s,
            Err(e) => {
                note(
                    log::Level::Warn,
                    format!("無法開啟服務 {SERVICE_NAME} 以檢查失敗復原設定：{e}"),
                );
                return;
            }
        };
        let desired = desired_recovery();
        let drift = recovery_drift(observe_recovery(&service).as_ref(), &desired);
        if drift.is_empty() {
            return;
        }
        let drift = drift.join("；");
        match set_failure_actions(&service, &desired) {
            Ok(()) => note(
                log::Level::Info,
                format!(
                    "已校正服務 {SERVICE_NAME} 的失敗復原設定（原本：{drift}）：重新啟動的延遲依序為 {:?} 秒，\
                     之後重複最後一個；「非 0 結束代碼也算失敗」的變更可能要到下次開機才生效",
                    recovery::RESTART_DELAYS_SECS
                ),
            ),
            Err(e) => note(
                log::Level::Warn,
                format!(
                    "服務 {SERVICE_NAME} 的失敗復原設定與預期不同（{drift}），而且無法修正：{e}；\
                     以 sc.exe qfailure {SERVICE_NAME} 檢查"
                ),
            ),
        }
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
        ensure_failure_actions();
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

    #[test]
    fn recovery_policy_restarts_with_a_capped_frequency() {
        // X1：最後一個動作會被無限重複，所以它決定長期的重試頻率
        let d = desired_recovery();
        assert_eq!(
            d.actions,
            [
                (RecoveryAction::Restart, 10_000),
                (RecoveryAction::Restart, 60_000),
                (RecoveryAction::Restart, 300_000)
            ]
        );
        assert_eq!(d.reset_secs, Some(86_400));
        assert!(d.on_non_crash_failure);
        // --help 的敘述與常數一致，而且不再出現「最多 3 次」
        for secs in recovery::RESTART_DELAYS_SECS {
            assert!(USAGE.contains(&format!("{secs} 秒")), "{secs}");
        }
        assert!(USAGE.contains("沒有次數上限") && !USAGE.contains("最多 3 次"));
    }

    #[test]
    fn recovery_drift_compares_the_whole_configuration() {
        let desired = desired_recovery();
        assert!(recovery_drift(Some(&desired), &desired).is_empty());
        // 讀不到 → 視為不一致
        assert_eq!(recovery_drift(None, &desired).len(), 1);
        // MSI 註冊的服務：沒有任何動作、旗標關閉
        let msi = RecoveryConfig {
            actions: Vec::new(),
            reset_secs: Some(0),
            on_non_crash_failure: false,
        };
        let d = recovery_drift(Some(&msi), &desired);
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d[0].contains("沒有失敗動作"), "{d:?}");
        // 0.8.0～0.10.1 註冊的服務：三個 10 秒（X1）——先前因為「已有動作」而不會被修正（X2）
        let legacy = RecoveryConfig {
            actions: vec![(RecoveryAction::Restart, 10_000); 3],
            ..desired.clone()
        };
        let d = recovery_drift(Some(&legacy), &desired);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].contains("Restart/10000ms"), "{d:?}");
        // 只有旗標不同、只有重設期不同、只有動作種類或個數不同，各自都算不一致
        let flag_off = RecoveryConfig {
            on_non_crash_failure: false,
            ..desired.clone()
        };
        assert_eq!(recovery_drift(Some(&flag_off), &desired).len(), 1);
        let never = RecoveryConfig {
            reset_secs: None,
            ..desired.clone()
        };
        assert_eq!(
            recovery_drift(Some(&never), &desired),
            ["重設期為永不".to_string()]
        );
        let mut stop_after_three = desired.clone();
        stop_after_three.actions.push((RecoveryAction::None, 0));
        assert_eq!(recovery_drift(Some(&stop_after_three), &desired).len(), 1);
        let mut reboot = desired.clone();
        reboot.actions[2].0 = RecoveryAction::Reboot;
        assert_eq!(recovery_drift(Some(&reboot), &desired).len(), 1);
    }

    #[cfg(not(windows))]
    #[test]
    fn other_platforms_refuse_with_a_usage_error() {
        assert_eq!(take_startup_note(), None);
        assert_eq!(run(&ServiceArgs::Uninstall), crate::EXIT_USAGE);
    }
}
