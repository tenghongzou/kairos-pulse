//! 本機狀態檔 `status.json`：執行中的 Pulse 寫出、`pulse status` 讀取（設計包 `pulse/cli-spec.md` §4.1）。
//!
//! - **寫入**：服務每 [`WRITE_INTERVAL`] 寫一次，狀態變化時另外寫（間隔至少 [`MIN_WRITE_GAP`]），
//!   以「同目錄的暫存檔（`create_new`、隨機檔名）→ rename 取代」原子更新：讀取端只會看到完整的舊檔或新檔。
//!   **不 fsync**：狀態檔只描述「現在」，斷電後下一次寫入（10 秒內）就會更新；每 10 秒 fsync
//!   一次在 eMMC／SD 卡的工控主機上是不必要的磨耗。斷電後少數檔案系統可能留下空檔，讀取端視同無法解析
//!   （本機狀態「未知」，服務啟動 10 秒內恢復）。
//! - **位置**（`cli-spec.md` §4.1）：`status.file` 未設定時，Linux 等為 `/var/lib/pulse/status.json`
//!   （systemd unit 的 `StateDirectory=pulse`），Windows 為 `%ProgramData%\Pulse\status.json`。
//!   無法寫入（例如沿用沒有 `StateDirectory=` 的舊 unit）時，改寫在工作目錄的 `logs/status.json`
//!   （[`LEGACY_RELATIVE`]），並在狀態中記 `PLS-E8204`，服務照常執行。
//! - **權限**：所有人可讀，`pulse status` 不需要 root。Unix 上檔案 0644、自行建立的目錄 0755，
//!   以 `set_permissions` 明確設定（unit 的 `UMask=0077` 會把建立時的權限位元拿掉）；Windows 在檔案上
//!   另外授予 BUILTIN\Users 讀取（目錄被管理者收緊時仍可讀）。
//! - **不含秘密**：私鑰、token、伺服器送來的未清理文字都不寫入；未啟用 mTLS 時 agent_id 就是身分憑據，
//!   不寫入（見 `local_status`）。
//! - **過時判斷**：寫入時記錄單調時鐘（Linux `/proc/uptime`、Windows `GetTickCount64`，開機後經過的時間），
//!   讀取端以同一個時鐘相減，不受系統時間調整或與 Kairos 的時鐘偏差影響（`cli-spec.md` §1.5）；
//!   取不到單調時鐘的平台退回比較牆鐘時間。
//!
//! 欄位名稱與 `pulse status --json` 的 `data` 一致（`cli-spec.md` §4.8）；只新增欄位，不改名、不改型別。
//! 每個欄位都有預設值，讀取端遇到較舊或較新的版本仍可解析。

use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// 狀態檔的 schema 名稱
pub const SCHEMA: &str = "pulse.status-file/v1";
/// 定期寫入的間隔（`cli-spec.md` §4.1：每 10 秒）
pub const WRITE_INTERVAL: Duration = Duration::from_secs(10);
/// 狀態變化觸發的寫入之間至少間隔這麼久（補送中每筆 ack 都會改變待補送筆數，不逐筆寫）
pub const MIN_WRITE_GAP: Duration = Duration::from_secs(1);
/// 超過這麼久沒有更新、而服務仍在執行時，判定為 agent 可能卡住（`PLS-W8004`）
pub const STALE_AFTER: Duration = Duration::from_secs(60);
/// 狀態檔大小上限：正常約 3～5 KB；超過時讀取端拒絕解析（避免被換成大檔案時整個讀進記憶體）
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Unix 上的狀態檔權限：所有人可讀（`cli-spec.md` §4.1）
#[cfg_attr(not(unix), allow(dead_code))]
const FILE_MODE: u32 = 0o644;
/// Unix 上自行建立狀態檔所在目錄時的權限（與 unit 的 `StateDirectoryMode=0755` 相同）
#[cfg_attr(not(unix), allow(dead_code))]
const DIR_MODE: u32 = 0o755;

/// 無法寫入預設位置時改寫的位置（相對於工作目錄；0.4 草稿之前的位置）
pub const LEGACY_RELATIVE: &str = "logs/status.json";

/// `status.file` 未設定時的位置：Linux 等 `/var/lib/pulse/status.json`；
/// Windows `%ProgramData%\Pulse\status.json`（取不到環境變數時為 `C:\ProgramData`）
pub fn default_path() -> std::path::PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramData")
            .filter(|v| !v.is_empty())
            .map_or_else(
                || std::path::PathBuf::from(r"C:\ProgramData"),
                std::path::PathBuf::from,
            );
        base.join("Pulse").join("status.json")
    } else {
        std::path::PathBuf::from("/var/lib/pulse/status.json")
    }
}

/// RFC 3339 UTC 毫秒（`2026-10-09T06:35:02.418Z`），`--json` 與狀態檔共用
pub fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 解析 RFC 3339 時間；無法解析時為 `None`
pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// 錯誤代碼與一句話說明
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeNote {
    pub code: String,
    pub message: String,
}

impl CodeNote {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// 狀態檔全文
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusFile {
    pub schema: String,
    /// 寫入時間（本機時鐘）
    pub updated_at: String,
    /// 寫入時的單調時鐘（開機後經過的毫秒）；取不到時為 `null`
    pub mono_ms: Option<u64>,
    /// 寫入這份檔案的 Pulse 行程
    pub pid: u32,
    pub started_at: String,
    /// 最近 1 小時內 Pulse 的啟動時間（含這次），用來計算「最近 1 小時重啟次數」
    pub starts_1h: Vec<String>,
    /// `false`：Pulse 已結束（結束前最後一次寫入）
    pub running: bool,
    pub stopped_at: Option<String>,
    pub exit_code: Option<i32>,
    /// 異常結束的原因（已是安全的單行文字）
    pub exit_message: Option<String>,
    pub version: String,
    pub commit: String,
    /// 未啟用 mTLS 時不寫（agent_id 等同身分憑據，而狀態檔所有人可讀），並設 `agent_id_hidden`
    pub agent_id: Option<String>,
    pub agent_id_hidden: bool,
    pub hostname: Option<String>,
    pub display_name: Option<String>,
    pub working_dir: Option<String>,
    /// 讀到的設定檔（絕對路徑）；不存在時為 `null`（只用內建預設值與環境變數）
    pub config_file: Option<String>,
    pub config: ConfigInfo,
    pub service: ServiceHint,
    pub report_interval_ms: u64,
    pub connection: Connection,
    pub reporting: Reporting,
    pub identity: Identity,
    pub clock: Clock,
}

/// 服務管理員的提示（Pulse 自己偵測到的）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceHint {
    /// `systemd`／`scm`；前景執行時為 `null`
    pub manager: Option<String>,
    /// 例如 `pulse.service`
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigInfo {
    /// 設定類警告（`PLS-W0302`、`PLS-W0303`、`PLS-W8101` 等）
    pub warnings: Vec<CodeNote>,
}

/// 與 Kairos 的連線
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Connection {
    /// `connecting`（啟動後尚未有結果）、`registered`、`reconnecting`（一般網路錯誤）、
    /// `rejected`（需要人工處理的錯誤，會持續重試）
    pub state: String,
    /// `host:port`
    pub server: String,
    /// 例如 `TLSv1.3`；未連線時為 `null`
    pub tls_version: Option<String>,
    pub mtls: bool,
    /// 是否驗證了伺服器憑證（`accept_invalid_certs = true` 時為 `false`）
    pub server_verified: bool,
    /// 協商的協定版本與 features（未連線時保留上次的值）
    pub proto: Option<u32>,
    pub features: Vec<String>,
    /// 進入目前狀態的時間：已註冊時為註冊時間，中斷時為中斷開始的時間
    pub since: Option<String>,
    pub consecutive_failures: u64,
    pub next_retry_at: Option<String>,
    pub last_error_code: Option<String>,
    /// 最近一次斷線原因（已清理的單行文字）
    pub last_error: Option<String>,
    pub last_registered_at: Option<String>,
    /// 上一次（已恢復的）中斷的開始與結束時間（本機緩衝曾滿的說明用）
    pub last_outage_from: Option<String>,
    pub last_outage_until: Option<String>,
    /// 最近一次需要人工處理的拒絕（成功註冊後清除）；之後即使只剩一般網路錯誤也保留
    pub last_rejection: Option<Rejection>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rejection {
    pub code: Option<String>,
    /// 第一次被拒的時間
    pub since: String,
    pub message: String,
}

/// 回報與本機緩衝
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reporting {
    pub last_report_at: Option<String>,
    pub last_ack_at: Option<String>,
    pub window_1h: Window1h,
    /// `durable`／`at_most_once`
    pub delivery: String,
    /// `server`／`disabled`／`unavailable`／`not_connected`；`durable` 時為 `null`
    pub delivery_reason: Option<String>,
    /// 實際的補送間隔（毫秒）
    pub replay_interval_ms: u64,
    pub buffer: Buffer,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Window1h {
    pub sampled: u64,
    pub acked: u64,
    pub buffered: u64,
    pub dropped_disconnected: u64,
    pub rejected_by_server: u64,
}

/// 本機緩衝（`cli-spec.md` §4.8 `data.reporting.buffer`）；`state = "disabled"` 時其餘欄位為 `null`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Buffer {
    pub enabled: bool,
    /// `ok`／`unavailable`／`disabled`
    pub state: String,
    pub dir: Option<String>,
    pub pending: Option<u64>,
    pub pending_bytes: Option<u64>,
    pub oldest_sample_at: Option<String>,
    pub max_bytes: Option<u64>,
    pub max_records: Option<u64>,
    pub max_age_hours: Option<u64>,
    pub usage_ratio: Option<f64>,
    pub replaying: Option<bool>,
    pub replay_eta_ms: Option<u64>,
    pub dropped_full: Option<u64>,
    pub dropped_expired: Option<u64>,
    pub dropped_rejected: Option<u64>,
    pub write_failures: Option<u64>,
    pub last_dropped_at: Option<String>,
    pub counters_since: Option<String>,
    // ---- 以下為狀態檔另外記錄、`pulse status` 組出文字用的欄位（`--json` 也照樣輸出） ----
    /// 最近一次因緩衝已滿而丟棄的時間
    pub last_dropped_full_at: Option<String>,
    /// 最近一次因超過保存時間而丟棄的時間
    pub last_dropped_expired_at: Option<String>,
    /// 因緩衝已滿而丟掉的取樣，取樣時間的範圍（最舊～最新）
    pub dropped_full_from: Option<String>,
    pub dropped_full_until: Option<String>,
    pub dropped_expired_from: Option<String>,
    pub dropped_expired_until: Option<String>,
    pub last_write_failure_at: Option<String>,
    /// 最近一次寫入失敗的原因（已是安全的單行文字）
    pub last_write_error: Option<String>,
    /// 無法開啟緩衝的原因（`state = "unavailable"`）
    pub error: Option<String>,
    /// Kairos 暫時無法保存（`storage_unavailable`）而退避時：下次重送的時間與代碼
    pub retry_at: Option<String>,
    pub retry_code: Option<String>,
}

/// 本機身分（用戶端憑證）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    pub agent_id_file: Option<String>,
    pub client_cert: Option<String>,
    /// 序號（十六進位，以 `:` 分隔每個位元組，例如 `10:02`）
    pub cert_serial: Option<String>,
    pub not_before: Option<String>,
    pub not_after: Option<String>,
    /// 權限過寬（`PLS-W8201`、`PLS-W8202`）等身分相關警告
    pub warnings: Vec<CodeNote>,
    /// 身分與註冊的狀態（設計包 `enrollment-ux.md` §4.2：`unenrolled`、`submitting`、`clock_rejected`、
    /// `pending_approval`、`issued`、`registered`、`rejected`、`cert_expired`…）；身分不是由
    /// `pulse enroll` 管理（手動憑證、未啟用 mTLS）時為 `null`
    pub state: Option<String>,
    /// 進行中或已結束的註冊申請（`cli-spec.md` §4.8 的 `identity.enrollment`）
    pub enrollment: Option<EnrollmentStatus>,
    /// 過期續簽（`identity.state = "cert_expired"` 時）
    pub grace: Option<GraceStatus>,
    /// 8443 正常輪替最近一次失敗的代碼（`PLS-W1111`）；成功或沒有輪替時為 `null`
    pub renew_failed_code: Option<String>,
    /// 下一次輪替的時點（已用壽命 2/3 或剩餘 `identity.renew_before_days`，取較早者）；不自動輪替時為 `null`
    pub renew_after: Option<String>,
}

/// 註冊申請（不含秘密：沒有 token、`poll_secret`）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnrollmentStatus {
    pub state: String,
    pub request_code: Option<String>,
    pub submitted_at: Option<String>,
    /// 申請的有效期限（Kairos 時間）
    pub expires_at: Option<String>,
    pub approved_by: Option<String>,
    pub rejected_reason: Option<String>,
    /// `clock_rejected`：本機減 Kairos（負值＝本機較慢）
    pub offset_ms: Option<i64>,
    pub local_time: Option<String>,
    pub server_time: Option<String>,
    pub next_submit_at: Option<String>,
    pub next_poll_at: Option<String>,
    pub last_code: Option<String>,
    pub observed_ip: Option<String>,
    pub enroll_server: Option<String>,
    pub token_prefix: Option<String>,
}

/// 過期續簽（`cli-spec.md` §4.8 的 `identity.grace`）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraceStatus {
    pub days: Option<u32>,
    /// 寬限期結束（依憑證的 `not_after` 加寬限天數；以 Kairos 的判斷為準）
    pub ends_at: Option<String>,
    pub renew_state: String,
    pub attempts: u32,
    pub last_attempt_at: Option<String>,
    pub next_attempt_at: Option<String>,
    pub last_code: Option<String>,
    pub offset_ms: Option<i64>,
    pub server_time: Option<String>,
    pub request_code: Option<String>,
}

/// 與 Kairos 的時鐘偏差（`cli-spec.md` §1.5）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Clock {
    /// 本機時間 − Kairos 時間（正值＝本機較快）；未知為 `null`
    pub offset_ms: Option<i64>,
    pub measured_at: Option<String>,
    /// 量測時的單調時鐘（判斷偏差是否仍有效：24 小時內、未重新開機）
    pub measured_mono_ms: Option<u64>,
    /// `hello_ack`／`server_info`／`token_check`／`enroll_result`／`clock_skew`／
    /// `server_info_unverified`；未知為 `null`
    pub source: Option<String>,
    /// 偏差不可用於到期判定（`server_info_unverified`：伺服器憑證的有效期間未驗證，ENROLL.md 第 3 節）
    pub unverified: bool,
}

/// 開機後經過的毫秒（單調，不受系統時間調整影響）；不支援的平台為 `None`
pub fn mono_ms() -> Option<u64> {
    imp::mono_ms()
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn mono_ms() -> Option<u64> {
        // /proc/uptime 第一欄為開機後經過的秒數（CLOCK_BOOTTIME，含睡眠時間），精確到 0.01 秒
        let text = std::fs::read_to_string("/proc/uptime").ok()?;
        parse_uptime(&text)
    }

    pub fn parse_uptime(text: &str) -> Option<u64> {
        let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
        (secs.is_finite() && secs >= 0.0).then_some((secs * 1000.0) as u64)
    }
}

#[cfg(windows)]
mod imp {
    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn GetTickCount64() -> u64;
    }

    pub fn mono_ms() -> Option<u64> {
        // SAFETY: 沒有參數、沒有副作用
        Some(unsafe { GetTickCount64() })
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn mono_ms() -> Option<u64> {
        None
    }
}

/// 原子寫入：同目錄的暫存檔以 `create_new`＋隨機檔名建立（不跟隨預先放置的符號連結、不覆寫既有檔案），
/// 寫入後 rename 取代。不 fsync（見模組說明）。失敗時刪除暫存檔。
pub fn write_atomically(path: &Path, content: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !dir.is_dir() {
        fs::DirBuilder::new().recursive(true).create(dir)?;
        // 建立時的權限受 umask 影響（unit 的 UMask=0077），另外明確設定
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "status.json".to_string());
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(FILE_MODE);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
        }
        file.write_all(content)?;
        drop(file);
        // 改名不改變檔案的 ACL：在暫存檔上授權，取代後的檔案就是一般使用者可讀
        #[cfg(windows)]
        if let Err(e) = win::grant_users_read(&tmp) {
            log::debug!("無法讓一般使用者讀取 {}：{e}", tmp.display());
        }
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(windows)]
mod win {
    //! 在檔案的 DACL 加上 BUILTIN\Users（S-1-5-32-545）的讀取權限
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;

    const SE_FILE_OBJECT: i32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    const ERROR_SUCCESS: u32 = 0;
    /// FILE_GENERIC_READ
    const FILE_GENERIC_READ: u32 = 0x0012_0089;
    /// ACCESS_MODE::GRANT_ACCESS
    const GRANT_ACCESS: i32 = 1;
    const NO_INHERITANCE: u32 = 0;
    /// TRUSTEE_FORM::TRUSTEE_IS_SID、TRUSTEE_TYPE::TRUSTEE_IS_WELL_KNOWN_GROUP
    const TRUSTEE_IS_SID: i32 = 0;
    const TRUSTEE_IS_WELL_KNOWN_GROUP: i32 = 5;

    #[repr(C)]
    struct TrusteeW {
        multiple_trustee: *mut c_void,
        multiple_trustee_operation: i32,
        trustee_form: i32,
        trustee_type: i32,
        name: *mut c_void,
    }

    #[repr(C)]
    struct ExplicitAccessW {
        access_permissions: u32,
        access_mode: i32,
        inheritance: u32,
        trustee: TrusteeW,
    }

    #[link(name = "advapi32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn ConvertStringSidToSidW(sid: *const u16, out: *mut *mut c_void) -> i32;
        fn GetNamedSecurityInfoW(
            name: *const u16,
            object_type: i32,
            info: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            sd: *mut *mut c_void,
        ) -> u32;
        fn SetEntriesInAclW(
            count: u32,
            entries: *const ExplicitAccessW,
            old: *mut c_void,
            new: *mut *mut c_void,
        ) -> u32;
        fn SetNamedSecurityInfoW(
            name: *const u16,
            object_type: i32,
            info: u32,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *mut c_void,
            sacl: *mut c_void,
        ) -> u32;
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    /// LocalAlloc 配置的記憶體，離開時釋放
    struct Local(*mut c_void);

    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 由 Win32 API 以 LocalAlloc 配置，只釋放一次
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    fn check(code: u32, what: &str) -> io::Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::other(format!("{what} 失敗（{code}）")))
        }
    }

    pub fn grant_users_read(path: &Path) -> io::Result<()> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let sid_text: Vec<u16> = "S-1-5-32-545"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: 字串以 NUL 結尾；輸出指標指向區域變數；配置的記憶體由 Local 釋放；
        // 舊的 DACL 指向 sd 之內，在 sd 釋放前使用完畢
        unsafe {
            let mut sid = std::ptr::null_mut();
            if ConvertStringSidToSidW(sid_text.as_ptr(), &mut sid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let sid = Local(sid);
            let mut old_dacl = std::ptr::null_mut();
            let mut sd = std::ptr::null_mut();
            check(
                GetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut old_dacl,
                    std::ptr::null_mut(),
                    &mut sd,
                ),
                "GetNamedSecurityInfoW",
            )?;
            let _sd = Local(sd);
            let entry = ExplicitAccessW {
                access_permissions: FILE_GENERIC_READ,
                access_mode: GRANT_ACCESS,
                inheritance: NO_INHERITANCE,
                trustee: TrusteeW {
                    multiple_trustee: std::ptr::null_mut(),
                    multiple_trustee_operation: 0,
                    trustee_form: TRUSTEE_IS_SID,
                    trustee_type: TRUSTEE_IS_WELL_KNOWN_GROUP,
                    name: sid.0,
                },
            };
            let mut new_dacl = std::ptr::null_mut();
            check(
                SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl),
                "SetEntriesInAclW",
            )?;
            let new_dacl = Local(new_dacl);
            check(
                SetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    new_dacl.0,
                    std::ptr::null_mut(),
                ),
                "SetNamedSecurityInfoW",
            )
        }
    }
}

/// systemd 設定的 `STATE_DIRECTORY`（可能以 `:` 分隔多個）的第一個絕對路徑
pub fn state_directory(value: Option<&std::ffi::OsStr>) -> Option<std::path::PathBuf> {
    let text = value?.to_str()?;
    let first = text.split(':').next()?.trim();
    let path = std::path::Path::new(first);
    (!first.is_empty() && path.is_absolute()).then(|| path.to_path_buf())
}

/// 狀態目錄對應的多實例編號：`…/pulse/N`（`pulse-N.service` 的 `StateDirectory=pulse/N`，N 為 1～999）；
/// 單一實例（`…/pulse`）或其他位置為 `None`
pub fn instance_of(dir: &std::path::Path) -> Option<u32> {
    let n: u32 = dir.file_name()?.to_str()?.parse().ok()?;
    let parent = dir.parent()?.file_name()?.to_str()?;
    ((1..=999).contains(&n) && parent == "pulse").then_some(n)
}

/// 多實例 N 的預設位置（`cli-spec.md` §2.3：`/var/lib/pulse/N/`）
pub fn instance_path(n: u32) -> std::path::PathBuf {
    let default = default_path();
    let dir = default
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    dir.join(n.to_string()).join("status.json")
}

/// 讀取狀態檔時的問題
#[derive(Debug)]
pub enum ReadError {
    /// 檔案不存在
    Missing,
    /// 沒有讀取權限
    PermissionDenied(io::Error),
    /// 其他 I/O 錯誤
    Io(io::Error),
    /// 內容無法解析（含空檔、過大、不是狀態檔）
    Invalid(String),
}

/// 讀取並解析狀態檔
pub fn read(path: &Path) -> Result<StatusFile, ReadError> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return Err(ReadError::PermissionDenied(e));
        }
        Err(e) => return Err(ReadError::Io(e)),
    };
    if meta.len() > MAX_FILE_BYTES {
        return Err(ReadError::Invalid(format!(
            "檔案過大（{} bytes）",
            meta.len()
        )));
    }
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return Err(ReadError::PermissionDenied(e));
        }
        Err(e) => return Err(ReadError::Io(e)),
    };
    parse(&bytes)
}

/// 解析狀態檔內容
pub fn parse(bytes: &[u8]) -> Result<StatusFile, ReadError> {
    let file: StatusFile =
        serde_json::from_slice(bytes).map_err(|e| ReadError::Invalid(e.to_string()))?;
    if !file.schema.starts_with("pulse.status-file/") {
        return Err(ReadError::Invalid(format!(
            "不是 Pulse 的狀態檔（schema「{}」）",
            file.schema.chars().take(64).collect::<String>()
        )));
    }
    Ok(file)
}

/// 狀態檔距今多久沒有更新：優先以單調時鐘相減（同一次開機時），否則以牆鐘相減（不為負）。
/// 單調時鐘倒退（寫入後重新開機過）時視為很久以前。
pub fn age(file: &StatusFile, now: DateTime<Utc>, now_mono: Option<u64>) -> Duration {
    if let (Some(then), Some(now_mono)) = (file.mono_ms, now_mono) {
        return match now_mono.checked_sub(then) {
            Some(ms) => Duration::from_millis(ms),
            None => Duration::MAX,
        };
    }
    parse_ts(&file.updated_at)
        .and_then(|t| (now - t).to_std().ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pulse-status-file-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> StatusFile {
        StatusFile {
            schema: SCHEMA.into(),
            updated_at: "2026-10-09T06:35:00.002Z".into(),
            mono_ms: Some(1_000_000),
            pid: 4151145,
            version: "0.3.0".into(),
            agent_id: Some("9422cb70-1b2c-4d3e-9f00-112233445566".into()),
            running: true,
            ..StatusFile::default()
        }
    }

    #[test]
    fn atomic_write_replaces_the_whole_file_and_leaves_no_temp_files() {
        let dir = temp_dir("write");
        let path = dir.join("logs").join("status.json");
        let first = serde_json::to_vec(&sample()).unwrap();
        write_atomically(&path, &first).unwrap();
        assert_eq!(fs::read(&path).unwrap(), first);
        let mut second = sample();
        second.pid = 7;
        let second = serde_json::to_vec(&second).unwrap();
        write_atomically(&path, &second).unwrap();
        assert_eq!(read(&path).unwrap().pid, 7);
        let names: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["status.json"], "暫存檔應已改名或刪除");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn status_file_is_world_readable_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("mode");
        let path = dir.join("state").join("status.json");
        write_atomically(&path, b"{}").unwrap();
        // 明確設定權限，不受 umask（systemd unit 的 UMask=0077）影響
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "{mode:o}");
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o755, "{dir_mode:o}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_location_follows_the_design() {
        let p = default_path();
        if cfg!(windows) {
            assert!(
                p.ends_with(std::path::Path::new("Pulse").join("status.json")),
                "{}",
                p.display()
            );
        } else {
            assert_eq!(p, std::path::Path::new("/var/lib/pulse/status.json"));
        }
    }

    #[test]
    fn instance_number_from_the_state_directory() {
        use std::path::Path;
        assert_eq!(instance_of(Path::new("/var/lib/pulse/2")), Some(2));
        assert_eq!(instance_of(Path::new("/var/lib/pulse")), None);
        assert_eq!(instance_of(Path::new("/var/lib/pulse/0")), None);
        assert_eq!(instance_of(Path::new("/var/lib/other/2")), None);
        assert_eq!(instance_of(Path::new("/var/lib/pulse/x")), None);
    }

    #[test]
    fn instance_and_state_directory_locations() {
        let default = default_path();
        let parent = default.parent().unwrap();
        assert_eq!(instance_path(2), parent.join("2").join("status.json"));
        if cfg!(unix) {
            assert_eq!(
                state_directory(Some(std::ffi::OsStr::new("/var/lib/pulse/2"))),
                Some(std::path::PathBuf::from("/var/lib/pulse/2"))
            );
            assert_eq!(
                state_directory(Some(std::ffi::OsStr::new("/var/lib/pulse:/var/lib/x"))),
                Some(std::path::PathBuf::from("/var/lib/pulse"))
            );
            assert_eq!(
                state_directory(Some(std::ffi::OsStr::new("relative"))),
                None
            );
        }
        assert_eq!(state_directory(Some(std::ffi::OsStr::new(""))), None);
        assert_eq!(state_directory(None), None);
    }

    #[cfg(windows)]
    #[test]
    fn users_can_read_the_status_file() {
        let dir = temp_dir("acl");
        let path = dir.join("status.json");
        write_atomically(&path, b"{}").unwrap();
        win::grant_users_read(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_reader_never_sees_a_partial_file() {
        // 寫入端反覆以不同大小的內容取代，讀取端每次讀到的都必須能完整解析
        let dir = temp_dir("race");
        let path = dir.join("status.json");
        write_atomically(&path, &serde_json::to_vec(&sample()).unwrap()).unwrap();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            for i in 0..300u32 {
                let mut s = sample();
                s.exit_message = Some("x".repeat((i as usize * 37) % 4000));
                s.pid = i;
                // Windows 上讀取端開著檔案時 rename 可能暫時失敗（下一次寫入會成功），略過
                let _ = write_atomically(&writer_path, &serde_json::to_vec(&s).unwrap());
            }
        });
        let mut reads = 0;
        while !writer.is_finished() || reads == 0 {
            match read(&path) {
                Ok(_) => reads += 1,
                Err(ReadError::Invalid(e)) => panic!("讀到不完整的狀態檔：{e}"),
                Err(_) => {}
            }
        }
        writer.join().unwrap();
        assert!(reads > 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_distinguishes_missing_invalid_and_foreign_files() {
        let dir = temp_dir("read");
        assert!(matches!(
            read(&dir.join("nope.json")),
            Err(ReadError::Missing)
        ));
        fs::write(dir.join("empty.json"), b"").unwrap();
        assert!(matches!(
            read(&dir.join("empty.json")),
            Err(ReadError::Invalid(_))
        ));
        fs::write(dir.join("other.json"), br#"{"schema":"kairos.x/v1"}"#).unwrap();
        assert!(matches!(
            read(&dir.join("other.json")),
            Err(ReadError::Invalid(_))
        ));
        // 不認得的欄位與缺少的欄位都能解析（前後版本相容）
        fs::write(
            dir.join("future.json"),
            br#"{"schema":"pulse.status-file/v1","pid":3,"new_field":{"a":1}}"#,
        )
        .unwrap();
        assert_eq!(read(&dir.join("future.json")).unwrap().pid, 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn age_prefers_the_monotonic_clock() {
        let file = sample();
        let now = parse_ts("2026-10-09T06:35:30.002Z").unwrap();
        // 單調時鐘：經過 5 秒（即使牆鐘差 30 秒，例如系統時間被調整過）
        assert_eq!(age(&file, now, Some(1_005_000)), Duration::from_secs(5));
        // 單調時鐘倒退：寫入後重新開機過 → 很久以前
        assert_eq!(age(&file, now, Some(10)), Duration::MAX);
        // 取不到單調時鐘：以牆鐘相減
        assert_eq!(age(&file, now, None), Duration::from_secs(30));
        let mut no_mono = file.clone();
        no_mono.mono_ms = None;
        assert_eq!(age(&no_mono, now, Some(5)), Duration::from_secs(30));
        // 牆鐘在未來（時鐘被往回調）→ 0，不 panic
        let past = parse_ts("2026-10-09T06:00:00Z").unwrap();
        assert_eq!(age(&no_mono, past, None), Duration::ZERO);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uptime_parsing() {
        assert_eq!(imp::parse_uptime("12345.67 54321.00\n"), Some(12_345_670));
        assert_eq!(imp::parse_uptime(""), None);
        assert_eq!(imp::parse_uptime("x 1"), None);
        assert!(mono_ms().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn tick_count_is_available() {
        assert!(mono_ms().unwrap() > 0);
    }
}
