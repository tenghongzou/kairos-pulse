//! 查詢服務管理員（systemd／Windows 服務控制管理員）與行程是否存在。
//!
//! `pulse status` 不需要 root：`systemctl show` 一般使用者即可執行；Windows 以 `SERVICE_QUERY_STATUS`
//! 開啟服務（Users 預設就有這個權限）。查不到（沒有 systemd、權限不足、逾時）時視為沒有服務管理員，
//! 改以狀態檔記錄的 PID 判斷 Pulse 是否在執行。

use std::time::Duration;

use chrono::{DateTime, Utc};

/// 服務管理員回報的狀態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvcState {
    Running,
    /// 正常停止（`inactive`、`SERVICE_STOPPED`）
    Stopped,
    /// 失敗（systemd `failed`，或停止時帶非 0 結束代碼）
    Failed,
    /// 啟動中、等待重新啟動（`activating`、`auto-restart`、`START_PENDING`）
    Starting,
    NotInstalled,
}

impl SvcState {
    /// `--json` 的 `service.state`
    pub fn as_str(self) -> &'static str {
        match self {
            SvcState::Running => "running",
            SvcState::Stopped => "stopped",
            SvcState::Failed => "failed",
            SvcState::Starting => "starting",
            SvcState::NotInstalled => "not_installed",
        }
    }
}

/// 查詢結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    /// `systemd`／`scm`
    pub manager: &'static str,
    /// systemd unit 名稱或 Windows 服務名稱
    pub unit: String,
    pub state: SvcState,
    pub main_pid: Option<u32>,
    /// 上次結束的代碼與時間
    pub exit_code: Option<i32>,
    pub exit_at: Option<DateTime<Utc>>,
}

/// 查詢服務；沒有可用的服務管理員時回傳 `None`
pub fn query(unit_hint: Option<&str>) -> Option<ServiceInfo> {
    imp::query(unit_hint)
}

/// 行程是否還在：`Some(true)` 在、`Some(false)` 不在、`None` 無法判斷
pub fn process_alive(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(false);
    }
    imp::process_alive(pid)
}

/// 預設的服務名稱（狀態檔沒有記錄時）
pub fn default_unit() -> &'static str {
    if cfg!(windows) {
        "Pulse"
    } else {
        "pulse.service"
    }
}

/// 解析 `systemctl show` 的輸出（`Key=Value` 每行一個）
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_systemctl_show(unit: &str, text: &str) -> Option<ServiceInfo> {
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
    };
    let load = get("LoadState")?;
    let active = get("ActiveState").unwrap_or("");
    let sub = get("SubState").unwrap_or("");
    let exit_code = get("ExecMainStatus").and_then(|v| v.parse::<i32>().ok());
    let exit_at = get("ExecMainExitTimestamp")
        .and_then(|v| v.strip_prefix('@'))
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&s| s > 0)
        .and_then(|s| DateTime::from_timestamp(s, 0));
    let state = match (load, active, sub) {
        ("not-found", ..) => SvcState::NotInstalled,
        (_, "active", _) | (_, "reloading", _) => SvcState::Running,
        (_, "failed", _) => SvcState::Failed,
        (_, "activating", _) | (_, _, "auto-restart") => SvcState::Starting,
        _ if exit_code.is_some_and(|c| c != 0) => SvcState::Failed,
        _ => SvcState::Stopped,
    };
    let main_pid = get("MainPID")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&p| p > 0);
    Some(ServiceInfo {
        manager: "systemd",
        unit: unit.to_string(),
        state,
        main_pid,
        exit_code: exit_code.filter(|_| state != SvcState::Running),
        exit_at: exit_at.filter(|_| state != SvcState::Running),
    })
}

/// 執行外部命令並取得 stdout；超過 `timeout` 時結束子行程並回傳 `None`
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn output_with_timeout(cmd: &mut std::process::Command, timeout: Duration) -> Option<String> {
    use std::io::Read as _;
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    /// systemctl 沒有回應（例如 D-Bus 卡住）時不等太久：SCCM 的探索指令碼 30 秒逾時
    const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);
    const PROPERTIES: &str =
        "--property=LoadState,ActiveState,SubState,MainPID,ExecMainStatus,ExecMainExitTimestamp";

    pub fn query(unit_hint: Option<&str>) -> Option<ServiceInfo> {
        // 不是以 systemd 開機的系統（容器等）：沒有服務管理員
        if !std::path::Path::new("/run/systemd/system").is_dir() {
            return None;
        }
        let unit = unit_hint.unwrap_or(default_unit());
        let show = |extra: &[&str]| {
            output_with_timeout(
                std::process::Command::new("systemctl")
                    .args(["show", unit, PROPERTIES, "--no-pager"])
                    .args(extra),
                SYSTEMCTL_TIMEOUT,
            )
        };
        // --timestamp=unix 需要 systemd 248 以上；較舊的版本不帶這個參數再試一次（沒有結束時間）
        let text = show(&["--timestamp=unix"]).or_else(|| show(&[]))?;
        parse_systemctl_show(unit, &text)
    }

    pub fn process_alive(pid: u32) -> Option<bool> {
        Some(std::path::Path::new(&format!("/proc/{pid}")).exists())
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;

    const SC_MANAGER_CONNECT: u32 = 0x0001;
    const SERVICE_QUERY_STATUS: u32 = 0x0004;
    const SC_STATUS_PROCESS_INFO: i32 = 0;
    const ERROR_SERVICE_DOES_NOT_EXIST: u32 = 1060;
    const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;
    const SERVICE_STOPPED: u32 = 1;
    const SERVICE_START_PENDING: u32 = 2;
    const SERVICE_RUNNING: u32 = 4;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_ACCESS_DENIED: u32 = 5;

    #[repr(C)]
    #[derive(Default)]
    struct ServiceStatusProcess {
        service_type: u32,
        current_state: u32,
        controls_accepted: u32,
        win32_exit_code: u32,
        service_specific_exit_code: u32,
        check_point: u32,
        wait_hint: u32,
        process_id: u32,
        service_flags: u32,
    }

    #[link(name = "advapi32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn OpenSCManagerW(machine: *const u16, database: *const u16, access: u32) -> *mut c_void;
        fn OpenServiceW(manager: *mut c_void, name: *const u16, access: u32) -> *mut c_void;
        fn QueryServiceStatusEx(
            service: *mut c_void,
            level: i32,
            buffer: *mut u8,
            size: u32,
            needed: *mut u32,
        ) -> i32;
        fn CloseServiceHandle(handle: *mut c_void) -> i32;
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn GetLastError() -> u32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    struct ScHandle(*mut c_void);

    impl Drop for ScHandle {
        fn drop(&mut self) {
            // SAFETY: 由 OpenSCManagerW／OpenServiceW 取得、非 NULL，只關閉一次
            unsafe {
                CloseServiceHandle(self.0);
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn query(unit_hint: Option<&str>) -> Option<ServiceInfo> {
        let unit = unit_hint.unwrap_or(default_unit()).to_string();
        // SAFETY: 參數為 NULL（本機、預設資料庫）或以 NUL 結尾的 UTF-16；回傳的 handle 由 ScHandle 關閉；
        // QueryServiceStatusEx 的緩衝區為大小正確的 SERVICE_STATUS_PROCESS
        unsafe {
            let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
            if manager.is_null() {
                return None;
            }
            let manager = ScHandle(manager);
            let name = wide(&unit);
            let service = OpenServiceW(manager.0, name.as_ptr(), SERVICE_QUERY_STATUS);
            if service.is_null() {
                if GetLastError() != ERROR_SERVICE_DOES_NOT_EXIST {
                    return None;
                }
                return Some(ServiceInfo {
                    manager: "scm",
                    unit,
                    state: SvcState::NotInstalled,
                    main_pid: None,
                    exit_code: None,
                    exit_at: None,
                });
            }
            let service = ScHandle(service);
            let mut status = ServiceStatusProcess::default();
            let mut needed = 0u32;
            let ok = QueryServiceStatusEx(
                service.0,
                SC_STATUS_PROCESS_INFO,
                (&mut status as *mut ServiceStatusProcess).cast(),
                std::mem::size_of::<ServiceStatusProcess>() as u32,
                &mut needed,
            );
            if ok == 0 {
                return None;
            }
            let exit = if status.win32_exit_code == ERROR_SERVICE_SPECIFIC_ERROR {
                status.service_specific_exit_code
            } else {
                status.win32_exit_code
            };
            let state = match status.current_state {
                SERVICE_RUNNING => SvcState::Running,
                SERVICE_START_PENDING => SvcState::Starting,
                SERVICE_STOPPED if exit != 0 => SvcState::Failed,
                SERVICE_STOPPED => SvcState::Stopped,
                // 停止中、暫停等：Pulse 不在正常執行
                _ => SvcState::Stopped,
            };
            Some(ServiceInfo {
                manager: "scm",
                unit,
                state,
                main_pid: (status.process_id != 0).then_some(status.process_id),
                exit_code: (state != SvcState::Running && exit != 0).then_some(exit as i32),
                exit_at: None,
            })
        }
    }

    pub fn process_alive(pid: u32) -> Option<bool> {
        // SAFETY: OpenProcess 失敗時回傳 NULL；成功時以 CloseHandle 關閉；code 指向區域變數
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                // 行程存在但沒有權限查詢（例如以 SYSTEM 執行的服務）：視為在執行
                return Some(GetLastError() == ERROR_ACCESS_DENIED);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            (ok != 0).then_some(code == STILL_ACTIVE)
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;

    pub fn query(_unit_hint: Option<&str>) -> Option<ServiceInfo> {
        None
    }

    pub fn process_alive(_pid: u32) -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemctl_show_is_parsed() {
        let running = "LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=4151145\n\
                       ExecMainStatus=0\nExecMainExitTimestamp=\n";
        let info = parse_systemctl_show("pulse.service", running).unwrap();
        assert_eq!(info.state, SvcState::Running);
        assert_eq!(info.main_pid, Some(4151145));
        assert_eq!(info.exit_code, None);

        let failed = "LoadState=loaded\nActiveState=failed\nSubState=failed\nMainPID=0\n\
                      ExecMainStatus=3\nExecMainExitTimestamp=@1791429693\n";
        let info = parse_systemctl_show("pulse.service", failed).unwrap();
        assert_eq!(info.state, SvcState::Failed);
        assert_eq!(info.main_pid, None);
        assert_eq!(info.exit_code, Some(3));
        assert_eq!(
            info.exit_at.unwrap().to_rfc3339(),
            "2026-10-08T03:21:33+00:00"
        );

        let restarting =
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nMainPID=0\n";
        assert_eq!(
            parse_systemctl_show("pulse.service", restarting)
                .unwrap()
                .state,
            SvcState::Starting
        );
        let missing = "LoadState=not-found\nActiveState=inactive\nSubState=dead\n";
        assert_eq!(
            parse_systemctl_show("pulse.service", missing)
                .unwrap()
                .state,
            SvcState::NotInstalled
        );
        let stopped = "LoadState=loaded\nActiveState=inactive\nSubState=dead\nExecMainStatus=0\n";
        assert_eq!(
            parse_systemctl_show("pulse.service", stopped)
                .unwrap()
                .state,
            SvcState::Stopped
        );
        assert_eq!(parse_systemctl_show("x", "garbage"), None);
        // 舊版 systemd 不支援 --timestamp=unix 時的時間格式：不解析
        let old = "LoadState=loaded\nActiveState=failed\nExecMainStatus=1\n\
                   ExecMainExitTimestamp=Fri 2026-10-09 14:01:33 CST\n";
        assert_eq!(parse_systemctl_show("x", old).unwrap().exit_at, None);
    }

    #[test]
    fn own_process_is_alive() {
        if cfg!(any(target_os = "linux", windows)) {
            assert_eq!(process_alive(std::process::id()), Some(true));
            assert_eq!(process_alive(0), Some(false));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_kills_hung_commands() {
        let started = std::time::Instant::now();
        let out = output_with_timeout(
            std::process::Command::new("sleep").arg("10"),
            Duration::from_millis(200),
        );
        assert_eq!(out, None);
        assert!(started.elapsed() < Duration::from_secs(5));
        let out = output_with_timeout(
            std::process::Command::new("echo").arg("ok"),
            Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Some("ok\n"));
    }
}
