//! 本機身分檔案（設計包 `enrollment-ux.md` §7、ENROLL.md 第 6.5 節）：
//!
//! ```text
//! identity/                 0700（Windows：只有 SYSTEM 與 Administrators）
//!   client.key              目前使用的私鑰（PKCS#8 PEM）          0600
//!   client.pem              用戶端憑證＋中繼憑證                    0640
//!   ca.pem                  8443／8444 信任的 Kairos CA            0640
//!   request.key             新註冊申請中的私鑰（核准後改名為 client.key） 0600
//!   grace.key               過期續簽申請中的新私鑰                  0600
//!   client.next.key/.pem    8443 正常輪替取得、尚未確認的新憑證       0600／0640
//!   enrollment.json         註冊與續簽的狀態（含 poll_secret）       0600
//!   .lock                   `pulse enroll` 與服務互斥（檔案鎖）
//! ```
//!
//! 寫入一律「同目錄暫存檔（`create_new`、隨機檔名、先設權限）→ 寫入 → fsync → rename →
//! fsync 目錄」：斷電後只會看到完整的舊檔或新檔。私鑰與 `enrollment.json` 在 Unix 為 0600、
//! Windows 為只有 SYSTEM 與 Administrators 可存取（不繼承上層目錄的 ACL）。
//!
//! Linux 上 `pulse enroll` 以 root 執行、服務以 `pulse` 帳號執行：`identity/` 已存在時，新寫入的
//! 檔案改為與 `identity/` 相同的擁有者（安裝時建立 `pulse:pulse 0700` 的目錄），服務才讀得到。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use kairos_proto::OsInfo;
use kairos_proto::enroll::{ApprovalDetail, CertPolicy, IssuedCertificate, ServerVerification};
use serde::{Deserialize, Serialize};

/// `enrollment.json` 的 schema
pub const ENROLLMENT_SCHEMA: &str = "pulse.enrollment/v1";
/// `enrollment.json` 的大小上限（正常約 2～6 KB；含已取得的憑證 PEM）
const MAX_ENROLLMENT_BYTES: u64 = 256 * 1024;

#[cfg_attr(not(unix), allow(dead_code))]
const DIR_MODE: u32 = 0o700;

/// 檔案的保護等級
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// 私鑰、`enrollment.json`：Unix 0600；Windows 只有 SYSTEM 與 Administrators
    Secret,
    /// 憑證：Unix 0640；Windows 沿用目錄的 ACL
    Certificate,
}

impl FileKind {
    #[cfg_attr(not(unix), allow(dead_code))]
    fn mode(self) -> u32 {
        match self {
            Self::Secret => 0o600,
            Self::Certificate => 0o640,
        }
    }
}

/// `identity/` 下的各檔案路徑
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPaths {
    pub dir: PathBuf,
}

impl IdentityPaths {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    pub fn client_key(&self) -> PathBuf {
        self.dir.join("client.key")
    }
    pub fn client_cert(&self) -> PathBuf {
        self.dir.join("client.pem")
    }
    pub fn ca(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }
    pub fn request_key(&self) -> PathBuf {
        self.dir.join("request.key")
    }
    pub fn grace_key(&self) -> PathBuf {
        self.dir.join("grace.key")
    }
    pub fn next_key(&self) -> PathBuf {
        self.dir.join("client.next.key")
    }
    pub fn next_cert(&self) -> PathBuf {
        self.dir.join("client.next.pem")
    }
    pub fn enrollment(&self) -> PathBuf {
        self.dir.join("enrollment.json")
    }
    pub fn lock(&self) -> PathBuf {
        self.dir.join(".lock")
    }

    /// 建立 `identity/`（0700）；已存在時不改權限
    pub fn ensure_dir(&self) -> io::Result<()> {
        if self.dir.is_dir() {
            return Ok(());
        }
        fs::DirBuilder::new().recursive(true).create(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(DIR_MODE))?;
        }
        #[cfg(windows)]
        if let Err(e) = win::restrict_to_admins(&self.dir, true) {
            log::warn!("無法限制 {} 的存取權限：{e}", self.dir.display());
        }
        Ok(())
    }

    /// 讀取 `enrollment.json`；不存在時 `Ok(None)`
    pub fn load_enrollment(&self) -> io::Result<Option<EnrollmentFile>> {
        let path = self.enrollment();
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if meta.len() > MAX_ENROLLMENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} 超過 {MAX_ENROLLMENT_BYTES} bytes", path.display()),
            ));
        }
        let bytes = fs::read(&path)?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            // 極少數檔案系統在斷電後留下空檔：視同沒有進行中的申請
            return Ok(None);
        }
        let file: EnrollmentFile = serde_json::from_slice(&bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} 無法解析：{e}", path.display()),
            )
        })?;
        if file.schema != ENROLLMENT_SCHEMA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} 的 schema「{}」不是 {ENROLLMENT_SCHEMA}",
                    path.display(),
                    file.schema
                ),
            ));
        }
        Ok(Some(file))
    }

    /// 原子寫入 `enrollment.json`（0600）
    pub fn save_enrollment(&self, file: &EnrollmentFile) -> io::Result<()> {
        let mut file = file.clone();
        file.schema = ENROLLMENT_SCHEMA.to_string();
        file.updated_at = crate::status_file::ts(chrono::Utc::now());
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        write_atomic(&self.enrollment(), &bytes, FileKind::Secret)
    }

    /// 取得 `identity/.lock` 的排他鎖；其他行程持有時回傳 `Ok(None)`
    pub fn try_lock(&self) -> io::Result<Option<IdentityLock>> {
        self.ensure_dir()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock())?;
        match file.try_lock() {
            Ok(()) => Ok(Some(IdentityLock { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// `identity/.lock` 的排他鎖（drop 時釋放）
#[derive(Debug)]
pub struct IdentityLock {
    _file: File,
}

/// 刪除檔案；不存在時視為成功
pub fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 原子取代：`from` 改名為 `to`（同一目錄），並 fsync 目錄
pub fn rename_atomic(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)?;
    sync_dir(to);
    Ok(())
}

/// 原子寫入（見模組說明）。目錄不存在時以 0700 建立
pub fn write_atomic(path: &Path, content: &[u8], kind: FileKind) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !dir.is_dir() {
        IdentityPaths::new(dir).ensure_dir()?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(kind.mode());
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(kind.mode()))?;
            match_dir_owner(&tmp, dir);
        }
        // 先收緊 ACL 再寫入內容：秘密不會有一刻落在繼承來的寬鬆 ACL 下
        #[cfg(windows)]
        if kind == FileKind::Secret {
            win::restrict_to_admins(&tmp, false)?;
        }
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        sync_dir(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// fsync 檔案所在的目錄（讓 rename 本身持久化）；Windows 沒有對應操作，略過
fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
        && let Ok(d) = File::open(dir)
    {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// root 寫入的檔案改為與所在目錄相同的擁有者（服務帳號才讀得到）。失敗只記 debug
#[cfg(unix)]
fn match_dir_owner(path: &Path, dir: &Path) {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(dir_meta) = fs::metadata(dir) else {
        return;
    };
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.uid() == dir_meta.uid() && meta.gid() == dir_meta.gid() {
        return;
    }
    if let Err(e) = std::os::unix::fs::chown(path, Some(dir_meta.uid()), Some(dir_meta.gid())) {
        log::debug!("無法變更 {} 的擁有者：{e}", path.display());
    }
}

/// Pulse 端的註冊狀態（設計包 `enrollment-ux.md` §4.1；`enrollment.json` 的 `state`）
pub mod state {
    /// 送出中（含 429／503 排隊、網路錯誤重試）
    pub const SUBMITTING: &str = "submitting";
    /// Kairos 因時鐘偏差暫不受理（`clock_skew`，每 5 分鐘重送）
    pub const CLOCK_REJECTED: &str = "clock_rejected";
    pub const PENDING_APPROVAL: &str = "pending_approval";
    /// 已取得憑證、尚未完成安裝或尚未連上 8443
    pub const ISSUED: &str = "issued";
    pub const REGISTERED: &str = "registered";
    pub const TOKEN_ERROR: &str = "token_error";
    pub const NOT_ACCEPTED: &str = "not_accepted";
    pub const SOURCE_BLOCKED: &str = "source_blocked";
    pub const SERVER_UNSUPPORTED: &str = "server_unsupported";
    pub const REJECTED: &str = "rejected";
    pub const REQUEST_EXPIRED: &str = "request_expired";
    /// 其他無法自動恢復的錯誤（`invalid_csr`、`license_exhausted`、`request_conflict`、
    /// Kairos 回應不合預期等）；`last_code` 寫出原因
    pub const FAILED: &str = "failed";

    /// 仍需自動重送或輪詢的狀態
    pub fn is_active(s: &str) -> bool {
        matches!(s, SUBMITTING | CLOCK_REJECTED | PENDING_APPROVAL | ISSUED)
    }
}

/// 過期續簽的子狀態（設計包 `enrollment-ux.md` §4.1；`enrollment.json` 的 `grace_renew_state`）
pub mod grace_state {
    pub const RENEWING: &str = "grace_renewing";
    pub const BLOCKED: &str = "grace_blocked";
    pub const PENDING: &str = "grace_pending";
    pub const EXCEEDED: &str = "grace_exceeded";
    pub const REJECTED: &str = "grace_rejected";
    pub const CLOCK_REJECTED: &str = "grace_clock_rejected";
    /// 舊憑證已撤銷（`certificate_revoked`，`PLS-E1105`）：不再續簽
    pub const REVOKED: &str = "revoked";
}

/// `identity/enrollment.json`（設計包 §7 的欄位，另加重送與輪詢需要的資料）。
///
/// 含 `poll_secret`（以及申請建立前暫存的 token），一律 0600 寫入。每個欄位都有預設值，讀取端遇到
/// 較舊或較新的版本仍可解析。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnrollmentFile {
    pub schema: String,
    pub updated_at: String,
    /// `enroll`（新註冊）／`grace_renewal`（過期續簽）
    pub kind: String,
    /// Pulse 端狀態（[`state`]）
    pub state: String,
    pub agent_id: String,
    /// agent 連線位址（8443），正規寫法
    pub server: Option<String>,
    /// 這次申請實際使用的註冊端點（正規寫法；簽章內容的最後一行）
    pub enroll_server: String,
    /// 註冊端點宣告了 `long_poll`（服務接續輪詢時使用）
    pub long_poll: bool,
    pub server_verification: Option<ServerVerification>,
    /// `ca_pin`／`ca_file`／`tofu_interactive`／`tofu_flag`（`pulse enroll --json` 的 `server_verified_by`）
    pub server_verified_by: Option<String>,
    pub ca_pin: Option<String>,
    /// 驗證 `issued` 回應的 `ca_pem` 用：信任錨的 CA 指紋（ENROLL.md 第 3 節「防止中途替換」）
    pub trusted_ca_pins: Vec<String>,
    /// 註冊 token：**只在申請建立前**暫存（429／503 排隊、時鐘偏差暫不受理時由服務重送），
    /// Kairos 受理（`pending_approval`／`issued`）或無法再使用時立即刪除
    pub token: Option<String>,
    /// token 秘密的前 4 碼（診斷用）
    pub token_prefix: Option<String>,
    /// 送出的 CSR（DER，base64url）：重送時沿用同一個
    pub csr: Option<String>,
    pub poll_secret: Option<String>,
    pub request_id: Option<String>,
    pub request_code: Option<String>,
    pub approval_detail: Option<ApprovalDetail>,
    pub submitted_at: Option<String>,
    pub expires_at: Option<String>,
    pub hostname: Option<String>,
    pub display_name: Option<String>,
    pub tags: Option<Vec<String>>,
    pub host_fingerprint: Option<String>,
    pub os: Option<OsInfo>,
    pub previous_serial: Option<String>,
    /// 送出次數（含重送）
    pub attempts: u32,
    pub last_code: Option<String>,
    pub last_error: Option<String>,
    /// 最近一次錯誤回應的 `error.code`
    pub protocol_code: Option<String>,
    pub observed_ip: Option<String>,
    pub scope: Option<String>,
    pub next_submit_at: Option<String>,
    pub next_poll_at: Option<String>,
    /// 輪詢的目前間隔（未加抖動）與 Kairos 最近一次給的 `poll_after_ms`
    pub poll_interval_ms: Option<u64>,
    pub poll_floor_ms: Option<u64>,
    /// 最近一次 `clock_skew` 回應：本機減 Kairos（負值＝本機較慢）
    pub offset_ms: Option<i64>,
    pub server_time: Option<String>,
    pub local_time: Option<String>,
    pub decided_by: Option<String>,
    pub decided_at: Option<String>,
    pub reject_reason: Option<String>,
    /// 已取得、尚未完成安裝的憑證（斷電後從這裡重新安裝）
    pub certificate: Option<IssuedCertificate>,
    pub policy: Option<CertPolicy>,
    pub cert_serial: Option<String>,
    pub cert_not_after: Option<String>,
    /// 過期寬限天數的快取（`policy.expired_renew_grace_days`、`hello_ack.expired_renew_grace_days`）
    pub grace_days: Option<u32>,
    pub grace_ends_at: Option<String>,
    pub grace_renew_state: Option<String>,
    pub grace_next_attempt_at: Option<String>,
    /// 最近一次過期續簽的 `sent_at`（同一個值不重複使用）
    pub grace_last_sent_at: Option<String>,
    pub grace_attempts: u32,
    pub grace_backoff_ms: Option<u64>,
    /// 8443 正常輪替（feature `renew`）進行中的申請
    pub rotation: Option<Rotation>,
}

/// 8443 正常輪替的進行中申請：重送時沿用同一個 `request_id` 與 CSR（ENROLL.md 第 13.1 節）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rotation {
    pub request_id: String,
    pub csr: String,
    pub started_at: String,
    pub attempts: u32,
    pub next_attempt_at: Option<String>,
    pub last_code: Option<String>,
    /// 1 → 12 小時退避的目前值（毫秒）
    pub backoff_ms: Option<u64>,
    /// 已取得新憑證、尚未以它完成 hello
    pub issued_serial: Option<String>,
}

impl EnrollmentFile {
    /// 目前的申請是否為過期續簽
    pub fn is_grace(&self) -> bool {
        self.kind == "grace_renewal"
    }
}

#[cfg(windows)]
mod win {
    //! 把檔案或目錄的 DACL 設為「只有 SYSTEM 與 Administrators 完全控制」並停止繼承
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;

    const SE_FILE_OBJECT: i32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
    const ERROR_SUCCESS: u32 = 0;
    const GENERIC_ALL: u32 = 0x1000_0000;
    const SET_ACCESS: i32 = 2;
    const NO_INHERITANCE: u32 = 0;
    /// OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    const SUB_CONTAINERS_AND_OBJECTS_INHERIT: u32 = 0x3;
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

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn sid(text: &str) -> io::Result<Local> {
        let w = wide(text);
        let mut out = std::ptr::null_mut();
        // SAFETY: w 以 0 結尾；out 由系統配置，交給 Local 釋放
        if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut out) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Local(out))
    }

    pub fn restrict_to_admins(path: &Path, is_dir: bool) -> io::Result<()> {
        let system = sid("S-1-5-18")?;
        let admins = sid("S-1-5-32-544")?;
        let inheritance = if is_dir {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        let entry = |s: &Local| ExplicitAccessW {
            access_permissions: GENERIC_ALL,
            access_mode: SET_ACCESS,
            inheritance,
            trustee: TrusteeW {
                multiple_trustee: std::ptr::null_mut(),
                multiple_trustee_operation: 0,
                trustee_form: TRUSTEE_IS_SID,
                trustee_type: TRUSTEE_IS_WELL_KNOWN_GROUP,
                name: s.0,
            },
        };
        let entries = [entry(&system), entry(&admins)];
        let mut acl = std::ptr::null_mut();
        // SAFETY: entries 在呼叫期間有效；新 ACL 由系統配置，交給 Local 釋放
        let code = unsafe { SetEntriesInAclW(2, entries.as_ptr(), std::ptr::null_mut(), &mut acl) };
        let acl = Local(acl);
        if code != ERROR_SUCCESS {
            return Err(io::Error::other(format!("SetEntriesInAclW 失敗（{code}）")));
        }
        let name: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: name 以 0 結尾；acl 為有效的 ACL
        let code = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl.0,
                std::ptr::null_mut(),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(io::Error::other(format!(
                "SetNamedSecurityInfoW 失敗（{code}）"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir()
            .join("pulse-enroll-store-tests")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn enrollment_round_trip_and_missing_file() {
        let paths = IdentityPaths::new(temp_dir().join("identity"));
        assert_eq!(paths.load_enrollment().unwrap(), None);
        let file = EnrollmentFile {
            kind: "enroll".into(),
            state: state::PENDING_APPROVAL.into(),
            agent_id: "6f1c0e4a-1b2c-4d3e-9f00-112233445566".into(),
            enroll_server: "kairos.example.gov.tw:8444".into(),
            poll_secret: Some("A165FG_KJYDbNpHsR6L9WLMOacQfetUwi-ZBnPdSrQg".into()),
            request_code: Some("1KGJ-N9XN".into()),
            ..Default::default()
        };
        paths.save_enrollment(&file).unwrap();
        let loaded = paths.load_enrollment().unwrap().unwrap();
        assert_eq!(loaded.schema, ENROLLMENT_SCHEMA);
        assert_eq!(loaded.state, file.state);
        assert_eq!(loaded.poll_secret, file.poll_secret);
        // 沒有遺留暫存檔
        let leftovers: Vec<_> = fs::read_dir(&paths.dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn interrupted_write_keeps_the_previous_file() {
        // 斷電模擬：暫存檔寫到一半（未 rename）時，讀取端只看到完整的舊檔；空檔視同沒有申請
        let paths = IdentityPaths::new(temp_dir().join("identity"));
        let old = EnrollmentFile {
            state: state::SUBMITTING.into(),
            ..Default::default()
        };
        paths.save_enrollment(&old).unwrap();
        fs::write(
            paths.dir.join(".enrollment.json.deadbeef.tmp"),
            br#"{"schema":"pulse.enrollment/v1","state":"pend"#,
        )
        .unwrap();
        assert_eq!(
            paths.load_enrollment().unwrap().unwrap().state,
            state::SUBMITTING
        );
        fs::write(paths.enrollment(), b"").unwrap();
        assert_eq!(paths.load_enrollment().unwrap(), None);
        fs::write(paths.enrollment(), b"{\"schema\":\"other/v9\"}").unwrap();
        assert!(paths.load_enrollment().is_err());
        fs::write(paths.enrollment(), b"{broken").unwrap();
        assert_eq!(
            paths.load_enrollment().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn secrets_are_0600_and_certificates_0640() {
        use std::os::unix::fs::PermissionsExt as _;
        let paths = IdentityPaths::new(temp_dir().join("identity"));
        write_atomic(&paths.client_key(), b"key", FileKind::Secret).unwrap();
        write_atomic(&paths.client_cert(), b"cert", FileKind::Certificate).unwrap();
        paths.save_enrollment(&EnrollmentFile::default()).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.dir), 0o700);
        assert_eq!(mode(&paths.client_key()), 0o600);
        assert_eq!(mode(&paths.enrollment()), 0o600);
        assert_eq!(mode(&paths.client_cert()), 0o640);
        // 覆寫既有檔案時權限不變
        write_atomic(&paths.client_key(), b"key2", FileKind::Secret).unwrap();
        assert_eq!(mode(&paths.client_key()), 0o600);
        assert_eq!(fs::read(paths.client_key()).unwrap(), b"key2");
    }

    #[test]
    fn lock_is_exclusive_between_handles() {
        let paths = IdentityPaths::new(temp_dir().join("identity"));
        let first = paths.try_lock().unwrap();
        assert!(first.is_some());
        assert!(paths.try_lock().unwrap().is_none(), "已被持有");
        drop(first);
        assert!(paths.try_lock().unwrap().is_some(), "釋放後可再取得");
    }
}
