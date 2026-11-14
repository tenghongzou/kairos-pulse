//! Windows 預設狀態目錄 `%ProgramData%\Pulse` 的信任檢查（安全稽核 2026-11 的 F4）。
//!
//! # 問題
//!
//! `%ProgramData%` 的預設 ACL 讓一般使用者可以建立子目錄。0.8.0～0.10.1 的服務（LocalSystem）在第一次
//! 啟動時才建立 `%ProgramData%\Pulse`，而且不檢查它：一般使用者搶先建立這個目錄（成為擁有者）之後，
//! 服務會在裡面寫 `status.json`、讀取並套用集中設定的快取 `remote\config.json`，`pulse status` 也會讀到
//! 使用者偽造的狀態檔。即使目錄是服務自己建立的，它繼承的 ACL 也讓一般使用者能在裡面建立檔案與子目錄。
//!
//! # 規則
//!
//! 使用預設狀態目錄之前（服務啟動時、`pulse status` 等命令讀取前）檢查目錄本身：
//!
//! 1. 是目錄，**不是重新解析點**（junction、符號連結、掛接點）；以 `FILE_FLAG_OPEN_REPARSE_POINT` 開啟，
//!    屬性、擁有者與 DACL 都從同一個控制代碼讀出。
//! 2. **擁有者**是 SYSTEM、Administrators 或 TrustedInstaller（擁有者一定能改 DACL）。
//! 3. **DACL** 不是 NULL，而且每一筆把寫入類權限（建立檔案或子目錄、刪除、改屬性、改 DACL、改擁有者…）
//!    給出去的允許項目，對象都是上述三者。只用於繼承的項目也算（它決定之後建立的檔案的 ACL）；
//!    `CREATOR OWNER`、`CREATOR GROUP`、`OWNER RIGHTS` 不算（對象就是擁有者或建立者）。
//!    只看允許項目、不計算拒絕項目與群組成員：可能多拒絕，不會漏放。
//!
//! 通過檢查之後，一般使用者無法再改動這個目錄（不能換成重新解析點、不能在裡面建立東西），所以檢查與
//! 使用之間沒有可利用的時間差。**不檢查**的部分：`%ProgramData%` 本身（它的預設 ACL 本來就讓使用者建立
//! 子目錄，必須信任作業系統的設定）、目錄內既有的檔案（處理方式是刪除整個目錄，見下）。
//!
//! 目錄不存在時，服務以明確的安全描述元建立（[`PROTECTED_SDDL`]：擁有者 Administrators；SYSTEM、
//! Administrators 完全控制並繼承；Users 只能列出與讀取目錄本身、不繼承）。唯讀的命令不建立目錄
//! （一般使用者執行 `pulse status` 不應該變成目錄的擁有者）。
//!
//! # 不合規時
//!
//! 拒絕使用（`PLS-E8207`）：狀態檔改寫在工作目錄的 `logs\status.json`，集中設定的快取改放 `logs\remote\`
//! （[`fallback_dir`]），`pulse status` 不讀 `%ProgramData%\Pulse\status.json`。服務照常執行。
//! 0.8.0～0.10.1 建立的目錄繼承了 `%ProgramData%` 的 ACL（Users 可在其中建立檔案），**也會被拒絕**：
//! 以系統管理員停止服務、`rd /s /q "%ProgramData%\Pulse"`（只有狀態檔與設定快取，都會重建）、啟動服務，
//! 服務會以正確的 ACL 重新建立它。
//!
//! 只適用於預設位置：`status.file` 明確指定的路徑與環境變數 `STATE_DIRECTORY` 指定的目錄由管理者負責。
//! Linux 等平台不檢查（`/var/lib/pulse` 由 systemd 的 `StateDirectory=` 以 root 建立）。
//!
//! 判斷邏輯（[`evaluate`]）是純函式，在任何平台都能測試；讀取安全描述元與建立目錄只在 Windows。

use std::path::PathBuf;

/// 新建預設狀態目錄時的安全描述元（SDDL）：擁有者 Administrators；DACL 不繼承上層（`P`），
/// SYSTEM 與 Administrators 完全控制（子目錄與檔案繼承），Users 讀取與列出（`0x1200a9` =
/// `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE`，只在目錄本身：`pulse status` 要能走到 `status.json`，
/// `remote\` 之下的設定快取則不給 Users 讀）
#[cfg_attr(not(windows), allow(dead_code))]
pub const PROTECTED_SDDL: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x1200a9;;;BU)";

/// 預設狀態目錄不合規時改用的目錄名稱（相對於工作目錄；與狀態檔的備用位置 `logs/status.json` 同一個目錄）
const FALLBACK_RELATIVE: &str = "logs";

/// 目錄不能使用的原因
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// 不是目錄
    NotDirectory,
    /// 是重新解析點（junction、符號連結、掛接點）
    ReparsePoint,
    /// 擁有者不是 SYSTEM、Administrators 或 TrustedInstaller（內容為擁有者的 SID）
    Owner(String),
    /// 沒有 DACL：任何人都有完全控制
    NullDacl,
    /// DACL 或 SID 的結構無法解析，或含有不認得的項目類型
    UnparsableAcl,
    /// DACL 讓這個 SID 寫入
    WritableBy(String),
    /// 無法讀取目錄的安全資訊
    Inspect(String),
    /// 目錄不存在而且無法以受保護的 ACL 建立
    Create(String),
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDirectory => write!(f, "不是目錄"),
            Self::ReparsePoint => write!(f, "是重新解析點（junction 或符號連結）"),
            Self::Owner(sid) => write!(
                f,
                "擁有者是 {sid}，不是 SYSTEM、Administrators 或 TrustedInstaller"
            ),
            Self::NullDacl => write!(f, "沒有 DACL（任何人都有完全控制）"),
            Self::UnparsableAcl => write!(f, "ACL 無法解析"),
            Self::WritableBy(sid) => write!(
                f,
                "ACL 讓 {sid} 寫入（只有 SYSTEM、Administrators、TrustedInstaller 可以有寫入權限）"
            ),
            Self::Inspect(e) => write!(f, "無法讀取擁有者與 ACL：{e}"),
            Self::Create(e) => write!(f, "不存在，而且無法以受保護的 ACL 建立：{e}"),
        }
    }
}

/// 目錄的安全屬性（Windows 上從同一個控制代碼讀出；測試時直接組出）
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// `FILE_ATTRIBUTE_DIRECTORY`
    pub directory: bool,
    /// `FILE_ATTRIBUTE_REPARSE_POINT`
    pub reparse_point: bool,
    /// 擁有者的 SID（二進位）；讀不到時為空
    pub owner: Vec<u8>,
    /// DACL（winnt.h 的 `ACL` 與其後的 ACE）；`None` 為 NULL DACL
    pub dacl: Option<Vec<u8>>,
}

/// 寫入類權限：`FILE_ADD_FILE`、`FILE_ADD_SUBDIRECTORY`、`FILE_WRITE_EA`、`FILE_DELETE_CHILD`、
/// `FILE_WRITE_ATTRIBUTES`（可用來設定重新解析點）、`DELETE`、`WRITE_DAC`、`WRITE_OWNER`、
/// `GENERIC_WRITE`、`GENERIC_ALL`
#[cfg_attr(not(windows), allow(dead_code))]
const WRITE_MASK: u32 = 0x0000_0002
    | 0x0000_0004
    | 0x0000_0010
    | 0x0000_0040
    | 0x0000_0100
    | 0x0001_0000
    | 0x0004_0000
    | 0x0008_0000
    | 0x4000_0000
    | 0x1000_0000;

/// ACE 類型（winnt.h）：允許、拒絕，以及兩者的 callback 版本。其他類型（物件 ACE、稽核…）不應出現在
/// 檔案系統的 DACL，遇到時視為無法解析
#[cfg_attr(not(windows), allow(dead_code))]
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x0;
#[cfg_attr(not(windows), allow(dead_code))]
const ACCESS_DENIED_ACE_TYPE: u8 = 0x1;
#[cfg_attr(not(windows), allow(dead_code))]
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 0x9;
#[cfg_attr(not(windows), allow(dead_code))]
const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 0xA;

/// 解析後的 SID
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sid {
    authority: u64,
    subs: Vec<u32>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Sid {
    /// `SID: Revision u8, SubAuthorityCount u8, IdentifierAuthority [u8; 6]（big endian）,
    /// SubAuthority [u32; n]（little endian）`；結構不合時為 `None`
    fn parse(bytes: &[u8]) -> Option<Self> {
        let (&revision, rest) = bytes.split_first()?;
        let (&count, rest) = rest.split_first()?;
        if revision != 1 || count > 15 {
            return None;
        }
        let authority = rest
            .get(..6)?
            .iter()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        let (chunks, _) = rest.get(6..6 + 4 * usize::from(count))?.as_chunks::<4>();
        let subs = chunks.iter().map(|c| u32::from_le_bytes(*c)).collect();
        Some(Self { authority, subs })
    }

    /// `S-1-5-32-544` 的寫法
    fn text(&self) -> String {
        let mut s = format!("S-1-{}", self.authority);
        for sub in &self.subs {
            s.push('-');
            s.push_str(&sub.to_string());
        }
        s
    }

    /// SYSTEM（S-1-5-18）、Administrators（S-1-5-32-544）、TrustedInstaller
    /// （S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464）
    fn is_trusted(&self) -> bool {
        matches!(
            (self.authority, self.subs.as_slice()),
            (5, [18])
                | (5, [32, 544])
                | (
                    5,
                    [
                        80,
                        956_008_885,
                        3_418_522_649,
                        1_831_038_044,
                        1_853_292_631,
                        2_271_478_464
                    ]
                )
        )
    }

    /// CREATOR OWNER（S-1-3-0）、CREATOR GROUP（S-1-3-1）、OWNER RIGHTS（S-1-3-4）：對象是物件的擁有者
    /// 或之後建立子物件的人，擁有者已另外檢查
    fn is_owner_placeholder(&self) -> bool {
        matches!((self.authority, self.subs.as_slice()), (3, [0 | 1 | 4]))
    }
}

/// 依規則判斷目錄能不能當狀態目錄（模組說明的規則 1～3）
#[cfg_attr(not(windows), allow(dead_code))]
pub fn evaluate(facts: &Facts) -> Result<(), Violation> {
    if facts.reparse_point {
        return Err(Violation::ReparsePoint);
    }
    if !facts.directory {
        return Err(Violation::NotDirectory);
    }
    match Sid::parse(&facts.owner) {
        Some(owner) if owner.is_trusted() => {}
        Some(owner) => return Err(Violation::Owner(owner.text())),
        None => return Err(Violation::Owner("（讀不到）".to_string())),
    }
    let Some(acl) = &facts.dacl else {
        return Err(Violation::NullDacl);
    };
    writable_by_untrusted(acl)
}

/// 走過 DACL 的每一筆 ACE（`ACL: AclRevision u8, Sbz1 u8, AclSize u16, AceCount u16, Sbz2 u16`；
/// `ACE: AceType u8, AceFlags u8, AceSize u16, Mask u32, SID…`）
#[cfg_attr(not(windows), allow(dead_code))]
fn writable_by_untrusted(acl: &[u8]) -> Result<(), Violation> {
    let u16_at = |b: &[u8], i: usize| -> Option<u16> {
        Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
    };
    let walk = || -> Option<Result<(), Violation>> {
        let size = usize::from(u16_at(acl, 2)?);
        let count = u16_at(acl, 4)?;
        let acl = acl.get(..size)?;
        let mut offset = 8usize;
        for _ in 0..count {
            let ace_type = *acl.get(offset)?;
            let ace_size = usize::from(u16_at(acl, offset + 2)?);
            if ace_size < 8 {
                return None;
            }
            let ace = acl.get(offset..offset.checked_add(ace_size)?)?;
            offset += ace_size;
            match ace_type {
                ACCESS_DENIED_ACE_TYPE | ACCESS_DENIED_CALLBACK_ACE_TYPE => continue,
                ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {}
                _ => return None,
            }
            let mask = u32::from_le_bytes(ace.get(4..8)?.try_into().ok()?);
            if mask & WRITE_MASK == 0 {
                continue;
            }
            let sid = Sid::parse(ace.get(8..)?)?;
            if !(sid.is_trusted() || sid.is_owner_placeholder()) {
                return Some(Err(Violation::WritableBy(sid.text())));
            }
        }
        Some(Ok(()))
    };
    walk().unwrap_or(Err(Violation::UnparsableAcl))
}

/// 檢查的時機
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 服務（或前景執行的 `pulse`）：目錄不存在時以 [`PROTECTED_SDDL`] 建立
    Service,
    /// 只讀取的命令（`pulse status`、`pulse diag`…）：不建立目錄；目錄不存在時視為通過（沒有東西可讀）
    ReadOnly,
}

/// 檢查 `dir` 能不能當狀態目錄。Windows 以外的平台一律通過
#[cfg(windows)]
pub fn check(dir: &std::path::Path, access: Access) -> Result<(), Violation> {
    use std::io::ErrorKind;
    let facts = match win::inspect(dir) {
        Ok(facts) => facts,
        Err(e) if e.kind() == ErrorKind::NotFound => match access {
            Access::ReadOnly => return Ok(()),
            Access::Service => {
                match win::create_protected(dir) {
                    Ok(()) => {}
                    // 剛好被別人建立了：照樣檢查它
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(Violation::Create(e.to_string())),
                }
                win::inspect(dir).map_err(|e| Violation::Inspect(e.to_string()))?
            }
        },
        // 讀不到安全資訊（含沒有權限）一律拒絕：擁有者可以讓別人讀不到它的 ACL，不能因此放行
        Err(e) => return Err(Violation::Inspect(e.to_string())),
    };
    evaluate(&facts)
}

/// 檢查 `dir` 能不能當狀態目錄。Windows 以外的平台一律通過
#[cfg(not(windows))]
#[cfg_attr(not(test), allow(dead_code))]
pub fn check(_dir: &std::path::Path, _access: Access) -> Result<(), Violation> {
    Ok(())
}

/// 需要檢查的預設狀態目錄：Windows 上沒有設定 `STATE_DIRECTORY` 時為 `%ProgramData%\Pulse`；
/// 其他情況（其他平台、管理者以 `STATE_DIRECTORY` 指定了目錄）為 `None`
pub fn guarded_default() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    if crate::status_file::state_directory(std::env::var_os("STATE_DIRECTORY").as_deref()).is_some()
    {
        return None;
    }
    crate::status_file::default_path()
        .parent()
        .map(std::path::Path::to_path_buf)
}

/// 服務啟動時的檢查結果（[`prepare`]；只做一次）
static PREPARED: std::sync::OnceLock<Result<(), Violation>> = std::sync::OnceLock::new();
/// 唯讀命令的檢查結果（[`verdict`]；只做一次）
static READ_ONLY: std::sync::OnceLock<Result<(), Violation>> = std::sync::OnceLock::new();

fn check_default(access: Access) -> Result<(), Violation> {
    match guarded_default() {
        Some(dir) => check(&dir, access),
        None => Ok(()),
    }
}

/// 服務啟動時呼叫一次：預設狀態目錄不存在時以受保護的 ACL 建立，再檢查。之後的 [`verdict`] 沿用這個結果
/// （即使先前已經有唯讀的檢查結果）
pub fn prepare() -> Result<(), Violation> {
    PREPARED
        .get_or_init(|| check_default(Access::Service))
        .clone()
}

/// 預設狀態目錄能不能用（不建立目錄）。不需要檢查的情況（其他平台、`STATE_DIRECTORY`）一律通過
pub fn verdict() -> Result<(), Violation> {
    if let Some(prepared) = PREPARED.get() {
        return prepared.clone();
    }
    READ_ONLY
        .get_or_init(|| check_default(Access::ReadOnly))
        .clone()
}

/// 預設狀態目錄只能由 [`prepare`] 以受保護的 ACL 建立：要建立的 `dir` 是它或在它之下、而它不存在時回傳
/// 錯誤，讓呼叫端不要以 `create_dir_all` 順手把它建出來（那樣會繼承 `%ProgramData%` 的 ACL，下次啟動被
/// 拒絕；在那之前一般使用者可以在裡面建立檔案）。其他情況（其他平台、其他目錄、已存在）通過
pub fn refuse_implicit_create(dir: &std::path::Path) -> std::io::Result<()> {
    match guarded_default() {
        Some(guarded) if dir.starts_with(&guarded) && !guarded.is_dir() => {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "狀態目錄 {} 不存在；它只在服務啟動時以受保護的 ACL 建立，請重新啟動服務",
                    guarded.display()
                ),
            ))
        }
        _ => Ok(()),
    }
}

/// 預設狀態目錄不合規時改用的目錄：工作目錄的 `logs`；目前目錄沒有 `logs` 而執行檔所在目錄有時
/// 用後者（在別的目錄執行 `pulse config show` 等命令時，找到服務實際使用的位置）
pub fn fallback_dir() -> PathBuf {
    let relative = PathBuf::from(FALLBACK_RELATIVE);
    if relative.is_dir() {
        return relative;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(FALLBACK_RELATIVE)))
        .filter(|d| d.is_dir())
        .unwrap_or(relative)
}

/// 拒絕使用預設狀態目錄時的訊息（日誌與狀態檔的設定警告共用）
pub fn rejection_message(violation: &Violation) -> String {
    let dir = guarded_default().unwrap_or_default();
    format!(
        "預設的狀態目錄 {dir} 不安全（{violation}），不使用：狀態檔改寫在工作目錄的 logs\\status.json，\
         集中設定的快取改放 logs\\remote\\。處理方式：以系統管理員停止服務，執行 rd /s /q \"{dir}\"\
         （其中只有狀態檔與設定快取，都會重建），再啟動服務——服務會以只有 SYSTEM 與 Administrators \
         可寫的 ACL 重新建立它",
        dir = dir.display()
    )
}

#[cfg(windows)]
mod win {
    //! 讀取目錄的屬性、擁有者與 DACL；以明確的安全描述元建立目錄。API 以 `raw-dylib` 直接連結
    //! （與 `file_perms`、`status_file` 相同，不引入新的 crate）
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;

    use super::Facts;

    const SE_FILE_OBJECT: i32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 0x1;
    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    const ERROR_SUCCESS: u32 = 0;
    const READ_CONTROL: u32 = 0x0002_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    /// FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    const FILE_SHARE_ALL: u32 = 0x7;
    const OPEN_EXISTING: u32 = 3;
    /// 開啟目錄需要
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// 開啟重新解析點本身，不跟隨
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const SDDL_REVISION_1: u32 = 1;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: i32,
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *const c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        /// `info` 指向 `BY_HANDLE_FILE_INFORMATION`（13 個 u32，第一個是 dwFileAttributes）
        fn GetFileInformationByHandle(handle: *mut c_void, info: *mut u32) -> i32;
        fn CreateDirectoryW(
            path: *const u16,
            security_attributes: *const SecurityAttributes,
        ) -> i32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "advapi32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            handle: *mut c_void,
            object_type: i32,
            security_info: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            security_descriptor: *mut *mut c_void,
        ) -> u32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl: *const u16,
            revision: u32,
            security_descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }

    /// 離開時關閉控制代碼
    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: 控制代碼由 CreateFileW 傳回且有效，只關閉一次
            unsafe {
                CloseHandle(self.0);
            }
        }
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

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// 以不跟隨重新解析點的方式開啟 `path`，從同一個控制代碼讀出屬性、擁有者與 DACL
    pub(super) fn inspect(path: &Path) -> io::Result<Facts> {
        let name = wide(path);
        // SAFETY: name 以 NUL 結尾；其餘參數為常數或 NULL
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                READ_CONTROL | FILE_READ_ATTRIBUTES,
                FILE_SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(handle);
        let mut info = [0u32; 13];
        // SAFETY: info 的大小與對齊符合 BY_HANDLE_FILE_INFORMATION（13 個 DWORD）
        if unsafe { GetFileInformationByHandle(handle.0, info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let attributes = info[0];
        let mut owner: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut c_void = std::ptr::null_mut();
        let mut sd: *mut c_void = std::ptr::null_mut();
        // SAFETY: 輸出指標都指向本函式的區域變數；不需要的輸出傳 NULL，API 允許
        let status = unsafe {
            GetSecurityInfo(
                handle.0,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        let _sd = Local(sd);
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // SAFETY: owner、dacl 指向 sd 之內（_sd 釋放前有效）。SID 的長度由標頭的 SubAuthorityCount
        // （offset 1）決定：8 + 4 × count；ACL 的長度在標頭的 AclSize（offset 2，可能未對齊）
        let (owner, dacl) = unsafe {
            let owner = if owner.is_null() {
                Vec::new()
            } else {
                let count = *owner.cast::<u8>().add(1);
                std::slice::from_raw_parts(owner.cast::<u8>(), 8 + 4 * usize::from(count)).to_vec()
            };
            let dacl = if dacl.is_null() {
                None
            } else {
                let size = std::ptr::read_unaligned(dacl.cast::<u8>().add(2).cast::<u16>());
                Some(std::slice::from_raw_parts(dacl.cast::<u8>(), usize::from(size)).to_vec())
            };
            (owner, dacl)
        };
        Ok(Facts {
            directory: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            reparse_point: attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
            owner,
            dacl,
        })
    }

    /// 以 [`super::PROTECTED_SDDL`] 建立目錄（上層目錄必須已存在）。已存在時回傳 `AlreadyExists`，
    /// 不修改既有目錄的 ACL
    pub(super) fn create_protected(path: &Path) -> io::Result<()> {
        let sddl: Vec<u16> = super::PROTECTED_SDDL
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut sd: *mut c_void = std::ptr::null_mut();
        // SAFETY: sddl 以 NUL 結尾；sd 指向區域變數，配置的記憶體由 Local 釋放
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sd = Local(sd);
        let attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            security_descriptor: sd.0,
            inherit_handle: 0,
        };
        let name = wide(path);
        // SAFETY: name 以 NUL 結尾；attributes 與其中的安全描述元在呼叫期間有效
        if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM: (u8, &[u32]) = (5, &[18]);
    const ADMINS: (u8, &[u32]) = (5, &[32, 544]);
    const USERS: (u8, &[u32]) = (5, &[32, 545]);
    const AUTH_USERS: (u8, &[u32]) = (5, &[11]);
    const EVERYONE: (u8, &[u32]) = (1, &[0]);
    const CREATOR_OWNER: (u8, &[u32]) = (3, &[0]);
    const TRUSTED_INSTALLER: (u8, &[u32]) = (
        5,
        &[
            80,
            956_008_885,
            3_418_522_649,
            1_831_038_044,
            1_853_292_631,
            2_271_478_464,
        ],
    );
    /// 某個本機的一般使用者
    const ALICE: (u8, &[u32]) = (5, &[21, 1111, 2222, 3333, 1001]);

    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    /// FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
    const READ_EXECUTE: u32 = 0x0012_00A9;
    /// `%ProgramData%` 預設給 Users 的 (CI)(WD,AD,WEA,WA)：建立檔案、建立子目錄、寫入延伸屬性與屬性
    const PROGRAMDATA_USERS_WRITE: u32 = 0x0000_0002 | 0x0000_0004 | 0x0000_0010 | 0x0000_0100;
    const OBJECT_INHERIT: u8 = 0x01;
    const CONTAINER_INHERIT: u8 = 0x02;
    const INHERIT_ONLY: u8 = 0x08;
    const INHERITED: u8 = 0x10;

    fn sid((authority, subs): (u8, &[u32])) -> Vec<u8> {
        let mut v = vec![
            1,
            u8::try_from(subs.len()).unwrap(),
            0,
            0,
            0,
            0,
            0,
            authority,
        ];
        for s in subs {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    fn ace(ace_type: u8, flags: u8, mask: u32, who: (u8, &[u32])) -> Vec<u8> {
        let who = sid(who);
        let size = u16::try_from(8 + who.len()).unwrap();
        let mut v = vec![ace_type, flags];
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&mask.to_le_bytes());
        v.extend_from_slice(&who);
        v
    }

    fn allow(flags: u8, mask: u32, who: (u8, &[u32])) -> Vec<u8> {
        ace(ACCESS_ALLOWED_ACE_TYPE, flags, mask, who)
    }

    fn acl(aces: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = aces.concat();
        let size = u16::try_from(8 + body.len()).unwrap();
        let count = u16::try_from(aces.len()).unwrap();
        let mut v = vec![2, 0];
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(&[0, 0]);
        v.extend_from_slice(&body);
        v
    }

    /// [`PROTECTED_SDDL`] 對應的 DACL
    fn protected_dacl() -> Vec<u8> {
        acl(&[
            allow(OBJECT_INHERIT | CONTAINER_INHERIT, FILE_ALL_ACCESS, SYSTEM),
            allow(OBJECT_INHERIT | CONTAINER_INHERIT, FILE_ALL_ACCESS, ADMINS),
            allow(0, READ_EXECUTE, USERS),
        ])
    }

    /// 0.8.0～0.10.1 的服務以預設值建立的目錄：繼承 `%ProgramData%` 的 ACL
    fn inherited_from_programdata() -> Vec<u8> {
        let ci_oi = OBJECT_INHERIT | CONTAINER_INHERIT | INHERITED;
        acl(&[
            allow(ci_oi, FILE_ALL_ACCESS, SYSTEM),
            allow(ci_oi, FILE_ALL_ACCESS, ADMINS),
            allow(ci_oi | INHERIT_ONLY, 0x1000_0000, CREATOR_OWNER),
            allow(ci_oi, READ_EXECUTE, USERS),
            allow(
                CONTAINER_INHERIT | INHERITED,
                PROGRAMDATA_USERS_WRITE,
                USERS,
            ),
        ])
    }

    fn dir(owner: (u8, &[u32]), dacl: Option<Vec<u8>>) -> Facts {
        Facts {
            directory: true,
            reparse_point: false,
            owner: sid(owner),
            dacl,
        }
    }

    #[test]
    fn the_directory_pulse_creates_passes() {
        assert_eq!(evaluate(&dir(ADMINS, Some(protected_dacl()))), Ok(()));
        // 擁有者是 SYSTEM 或 TrustedInstaller 也可以
        assert_eq!(evaluate(&dir(SYSTEM, Some(protected_dacl()))), Ok(()));
        assert_eq!(
            evaluate(&dir(TRUSTED_INSTALLER, Some(protected_dacl()))),
            Ok(())
        );
        // 只用於繼承的 CREATOR OWNER 項目不算（%ProgramFiles% 之下的目錄都有）
        let with_creator_owner = acl(&[
            allow(INHERITED, FILE_ALL_ACCESS, SYSTEM),
            allow(INHERITED, FILE_ALL_ACCESS, ADMINS),
            allow(
                OBJECT_INHERIT | CONTAINER_INHERIT | INHERIT_ONLY | INHERITED,
                0x1000_0000,
                CREATOR_OWNER,
            ),
            allow(INHERITED, READ_EXECUTE, USERS),
            allow(INHERITED, FILE_ALL_ACCESS, TRUSTED_INSTALLER),
        ]);
        assert_eq!(evaluate(&dir(SYSTEM, Some(with_creator_owner))), Ok(()));
        // SDDL 與上面的 DACL 描述的是同一組項目
        assert!(PROTECTED_SDDL.starts_with("O:BAD:PAI"));
        for entry in ["(A;OICI;FA;;;SY)", "(A;OICI;FA;;;BA)", "(A;;0x1200a9;;;BU)"] {
            assert!(PROTECTED_SDDL.contains(entry), "{entry}");
        }
        assert_eq!(PROTECTED_SDDL.matches("(A;").count(), 3);
        assert_eq!(READ_EXECUTE & WRITE_MASK, 0);
    }

    /// F4 的回歸測試：一般使用者搶先建立的目錄（擁有者是使用者）被拒絕，不論它的 DACL 看起來多嚴格
    #[test]
    fn a_directory_owned_by_a_standard_user_is_rejected() {
        let v = evaluate(&dir(ALICE, Some(protected_dacl()))).unwrap_err();
        assert_eq!(v, Violation::Owner("S-1-5-21-1111-2222-3333-1001".into()));
        assert!(v.to_string().contains("擁有者是 S-1-5-21-"), "{v}");
        // 擁有者讀不到或 SID 損毀：拒絕
        let mut broken = dir(ADMINS, Some(protected_dacl()));
        broken.owner.clear();
        assert!(matches!(evaluate(&broken), Err(Violation::Owner(_))));
        broken.owner = vec![1, 5, 0, 0, 0, 0, 0, 5, 18, 0];
        assert!(matches!(evaluate(&broken), Err(Violation::Owner(_))));
        // Users 群組、Everyone 當擁有者也不行
        for owner in [USERS, EVERYONE, AUTH_USERS, CREATOR_OWNER] {
            assert!(
                matches!(
                    evaluate(&dir(owner, Some(protected_dacl()))),
                    Err(Violation::Owner(_))
                ),
                "{owner:?}"
            );
        }
    }

    /// F4 的回歸測試：重新解析點（junction、符號連結）被拒絕，即使擁有者與 DACL 都合規
    #[test]
    fn a_reparse_point_is_rejected() {
        let mut junction = dir(ADMINS, Some(protected_dacl()));
        junction.reparse_point = true;
        assert_eq!(evaluate(&junction), Err(Violation::ReparsePoint));
        // 指向檔案的符號連結沒有目錄屬性：仍以重新解析點回報
        junction.directory = false;
        assert_eq!(evaluate(&junction), Err(Violation::ReparsePoint));
        // 一般檔案佔了這個名稱
        let mut file = dir(ADMINS, Some(protected_dacl()));
        file.directory = false;
        assert_eq!(evaluate(&file), Err(Violation::NotDirectory));
    }

    /// F4 的回歸測試：DACL 讓一般使用者寫入的目錄被拒絕
    #[test]
    fn a_directory_writable_by_standard_users_is_rejected() {
        // 舊版服務建立的目錄（繼承 %ProgramData%：Users 可建立檔案與子目錄）
        assert_eq!(
            evaluate(&dir(ADMINS, Some(inherited_from_programdata()))),
            Err(Violation::WritableBy("S-1-5-32-545".into()))
        );
        // 每一種寫入類權限單獨出現都算
        for bit in [
            0x2u32,
            0x4,
            0x10,
            0x40,
            0x100,
            0x1_0000,
            0x4_0000,
            0x8_0000,
            0x4000_0000,
            0x1000_0000,
        ] {
            for who in [USERS, AUTH_USERS, EVERYONE, ALICE] {
                let dacl = acl(&[
                    allow(0, FILE_ALL_ACCESS, SYSTEM),
                    allow(0, READ_EXECUTE | bit, who),
                ]);
                assert!(
                    matches!(
                        evaluate(&dir(SYSTEM, Some(dacl))),
                        Err(Violation::WritableBy(_))
                    ),
                    "{bit:#x} {who:?}"
                );
            }
        }
        // 只用於繼承的項目也算：之後由服務建立的檔案會繼承到 Users 的寫入權限
        let inherit_only = acl(&[
            allow(0, FILE_ALL_ACCESS, SYSTEM),
            allow(
                OBJECT_INHERIT | CONTAINER_INHERIT | INHERIT_ONLY,
                0x4000_0000,
                USERS,
            ),
        ]);
        assert_eq!(
            evaluate(&dir(SYSTEM, Some(inherit_only))),
            Err(Violation::WritableBy("S-1-5-32-545".into()))
        );
        // callback 版本的允許項目也算
        let callback = acl(&[ace(ACCESS_ALLOWED_CALLBACK_ACE_TYPE, 0, 0x2, EVERYONE)]);
        assert_eq!(
            evaluate(&dir(SYSTEM, Some(callback))),
            Err(Violation::WritableBy("S-1-1-0".into()))
        );
        // 拒絕項目不能用來放行：同時有「拒絕 Users 寫入」與「允許 Users 寫入」仍然拒絕
        let deny_then_allow = acl(&[
            ace(ACCESS_DENIED_ACE_TYPE, 0, FILE_ALL_ACCESS, USERS),
            allow(0, FILE_ALL_ACCESS, USERS),
            allow(0, FILE_ALL_ACCESS, ADMINS),
        ]);
        assert!(matches!(
            evaluate(&dir(ADMINS, Some(deny_then_allow))),
            Err(Violation::WritableBy(_))
        ));
        // 只有拒絕項目與受信任對象的允許項目：通過
        let deny_only = acl(&[
            ace(ACCESS_DENIED_ACE_TYPE, 0, FILE_ALL_ACCESS, USERS),
            allow(0, FILE_ALL_ACCESS, ADMINS),
        ]);
        assert_eq!(evaluate(&dir(ADMINS, Some(deny_only))), Ok(()));
        // 一般使用者只有讀取：通過
        let read_only = acl(&[
            allow(0, FILE_ALL_ACCESS, SYSTEM),
            allow(OBJECT_INHERIT | CONTAINER_INHERIT, READ_EXECUTE, USERS),
            allow(0, READ_EXECUTE, ALICE),
        ]);
        assert_eq!(evaluate(&dir(SYSTEM, Some(read_only))), Ok(()));
    }

    #[test]
    fn null_and_malformed_dacls_are_rejected() {
        assert_eq!(evaluate(&dir(ADMINS, None)), Err(Violation::NullDacl));
        let good = protected_dacl();
        for bad in [
            Vec::new(),
            good[..7].to_vec(),
            good[..good.len() - 1].to_vec(), // AclSize 超出實際長度
            {
                let mut b = good.clone();
                b[10] = 4; // AceSize 比標頭還小
                b[11] = 0;
                b
            },
            {
                let mut b = good.clone();
                b[4] = 9; // AceCount 多於實際
                b
            },
            // 不認得的 ACE 類型（物件 ACE）：不猜它的結構
            acl(&[ace(0x5, 0, FILE_ALL_ACCESS, ADMINS)]),
            // 允許項目的 SID 截斷
            {
                let mut b = acl(&[allow(0, FILE_ALL_ACCESS, ALICE)]);
                b[8 + 8 + 1] = 15; // SubAuthorityCount 超出 ACE
                b
            },
        ] {
            assert_eq!(
                evaluate(&dir(ADMINS, Some(bad.clone()))),
                Err(Violation::UnparsableAcl),
                "{bad:?}"
            );
        }
        // 空的 DACL（沒有任何項目）：沒有人有權限，通過
        assert_eq!(evaluate(&dir(ADMINS, Some(acl(&[])))), Ok(()));
    }

    #[test]
    fn sid_text_and_messages() {
        assert_eq!(Sid::parse(&sid(USERS)).unwrap().text(), "S-1-5-32-545");
        assert_eq!(Sid::parse(&sid(EVERYONE)).unwrap().text(), "S-1-1-0");
        assert!(Sid::parse(&sid(TRUSTED_INSTALLER)).unwrap().is_trusted());
        assert!(!Sid::parse(&sid(USERS)).unwrap().is_trusted());
        assert_eq!(Sid::parse(&[2, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]), None);
        for v in [
            Violation::NotDirectory,
            Violation::ReparsePoint,
            Violation::Owner("S-1-5-21-1-2-3-1001".into()),
            Violation::NullDacl,
            Violation::UnparsableAcl,
            Violation::WritableBy("S-1-5-32-545".into()),
            Violation::Inspect("x".into()),
            Violation::Create("x".into()),
        ] {
            assert!(!v.to_string().is_empty());
            let msg = rejection_message(&v);
            assert!(msg.contains("不安全") && msg.contains("rd /s /q"), "{msg}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn other_platforms_are_not_checked() {
        assert_eq!(guarded_default(), None);
        assert_eq!(prepare(), Ok(()));
        assert_eq!(verdict(), Ok(()));
        assert_eq!(
            check(std::path::Path::new("/nonexistent"), Access::Service),
            Ok(())
        );
        assert_eq!(fallback_dir().file_name().unwrap(), "logs");
        assert!(refuse_implicit_create(std::path::Path::new("/var/lib/pulse/remote")).is_ok());
    }

    /// 在 Windows 上以實際的目錄驗證（CI 的 Windows runner 以系統管理員執行）
    #[cfg(windows)]
    mod on_windows {
        use super::super::*;

        fn temp(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pulse-state-dir-{tag}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn run(program: &str, args: &[&std::ffi::OsStr]) -> bool {
            std::process::Command::new(program)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }

        #[test]
        fn read_only_check_never_creates_the_directory() {
            let base = temp("readonly");
            let target = base.join("Pulse");
            assert_eq!(check(&target, Access::ReadOnly), Ok(()));
            assert!(!target.exists());
            // 不在預設狀態目錄之下的路徑不受「不得順手建立」的限制
            assert!(refuse_implicit_create(&target).is_ok());
            std::fs::remove_dir_all(&base).unwrap();
        }

        #[test]
        fn an_ordinary_directory_under_temp_is_rejected() {
            // %TEMP% 之下的目錄繼承使用者設定檔的 ACL（使用者本人可寫），不是受信任的狀態目錄
            let base = temp("plain");
            let target = base.join("Pulse");
            std::fs::create_dir(&target).unwrap();
            for access in [Access::ReadOnly, Access::Service] {
                let v = check(&target, access).unwrap_err();
                assert!(
                    matches!(v, Violation::Owner(_) | Violation::WritableBy(_)),
                    "{v:?}"
                );
            }
            std::fs::remove_dir_all(&base).unwrap();
        }

        #[test]
        fn the_service_creates_a_protected_directory_and_rejects_it_once_loosened() {
            let base = temp("create");
            let target = base.join("Pulse");
            match check(&target, Access::Service) {
                Ok(()) => {}
                // 不是以系統管理員執行：無法把擁有者設為 Administrators，建立失敗（不留下目錄）
                Err(Violation::Create(e)) => {
                    eprintln!("略過：不是系統管理員（{e}）");
                    assert!(!target.exists());
                    std::fs::remove_dir_all(&base).unwrap();
                    return;
                }
                Err(v) => panic!("新建的目錄應該通過：{v:?}"),
            }
            assert!(target.is_dir());
            // 再檢查一次（已存在）仍通過，唯讀的檢查也通過
            assert_eq!(check(&target, Access::Service), Ok(()));
            assert_eq!(check(&target, Access::ReadOnly), Ok(()));
            // 之後建立的子目錄只繼承 SYSTEM 與 Administrators：同樣通過
            let child = target.join("remote");
            std::fs::create_dir(&child).unwrap();
            assert_eq!(check(&child, Access::ReadOnly), Ok(()));
            // 讓 Users 可以修改 → 拒絕
            assert!(run(
                "icacls",
                &[
                    target.as_os_str(),
                    "/grant".as_ref(),
                    "*S-1-5-32-545:(M)".as_ref()
                ]
            ));
            assert_eq!(
                check(&target, Access::Service),
                Err(Violation::WritableBy("S-1-5-32-545".into()))
            );
            std::fs::remove_dir_all(&base).unwrap();
        }

        #[test]
        fn a_junction_is_rejected_without_following_it() {
            let base = temp("junction");
            let real = base.join("real");
            let link = base.join("Pulse");
            std::fs::create_dir(&real).unwrap();
            if !run(
                "cmd",
                &[
                    "/c".as_ref(),
                    "mklink".as_ref(),
                    "/J".as_ref(),
                    link.as_os_str(),
                    real.as_os_str(),
                ],
            ) {
                eprintln!("略過：無法建立 junction");
                std::fs::remove_dir_all(&base).unwrap();
                return;
            }
            for access in [Access::ReadOnly, Access::Service] {
                assert_eq!(check(&link, access), Err(Violation::ReparsePoint));
            }
            // 先移除 junction 本身（不跟隨），再清掉其餘
            std::fs::remove_dir(&link).unwrap();
            std::fs::remove_dir_all(&base).unwrap();
        }
    }
}
