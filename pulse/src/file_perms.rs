//! 敏感檔案（`agent.id`、`server.client_key`）的權限檢查：權限過寬時回傳警告訊息，
//! 由呼叫端記 WARN（不拒絕啟動、不自動修改權限）。
//!
//! # Unix
//!
//! 依檔案的權限位元與擁有者判斷（與 PostgreSQL 檢查 `ssl_key_file` 的規則相同）：
//!
//! - 其他使用者（other）有任何權限 → 警告。
//! - 群組（group）有寫入或執行權限 → 警告。
//! - 群組可讀：擁有者是 root 時允許（INSTALL 的建議配置 `root:pulse 640`，服務帳號只能讀、
//!   不能改）；擁有者不是 root 時警告（例如 `pulse:pulse 640`，同群組的其他帳號也能讀）。
//!
//! # Windows
//!
//! 讀取檔案的 DACL（`GetNamedSecurityInfoW`），找出授權給「一般使用者」這類廣泛群組的
//! 允許項目（ACE）：Everyone、Authenticated Users、BUILTIN\Users、INTERACTIVE、
//! ANONYMOUS LOGON、本機帳戶（Local account）、Guests、Domain Users、Domain Guests。
//! 這些群組可讀取或寫入檔案時警告；DACL 為 NULL（任何人都有完全控制）也警告。
//!
//! 限制：
//!
//! - 只看「允許」項目，不計算「拒絕」項目與實際有效權限（例如同時有拒絕 Users 讀取的項目時
//!   仍會警告），因此可能多報、不會漏報上述群組的直接授權。
//! - 不展開群組成員：授權給某個自訂群組或其他個別帳號不會警告。
//! - 檔案權限通常繼承自所在目錄。`C:\`、`C:\ProgramData`、`C:\Program Files` 下新建的檔案
//!   預設讓 Users 可讀，因此放在這些位置會收到警告；應放在只有執行帳號、SYSTEM 與
//!   Administrators 可存取的目錄，或以 `icacls` 移除繼承後只授權給這些帳號。
//!
//! Windows 的 API 宣告直接以 `raw-dylib` 連結 `advapi32.dll`／`kernel32.dll`，不引入新的
//! crate（lock 中的 `windows-sys` 未啟用 `Win32_Security_Authorization` feature）。
//! ACL 的二進位結構以安全的 Rust 解析（[`parse_acl`]），可在任何平台測試。

use std::path::Path;

/// 要檢查的敏感檔案：顯示名稱與「為什麼不能讓別人讀」的說明。
#[derive(Debug, Clone, Copy)]
pub struct SensitiveFile {
    /// 日誌中的名稱，例如「agent_id 檔案」
    pub name: &'static str,
    /// 權限過寬的風險說明
    pub risk: &'static str,
}

/// `agent.id`：未啟用 mTLS 時就是身分憑據。
pub const AGENT_ID_FILE: SensitiveFile = SensitiveFile {
    name: "agent_id 檔案",
    risk: "未啟用 mTLS 時 agent_id 等同身分憑據（可寫入者還能竄改本機身分）",
};

/// `server.client_key`：mTLS 客戶端私鑰。
pub const CLIENT_KEY_FILE: SensitiveFile = SensitiveFile {
    name: "客戶端私鑰 server.client_key",
    risk: "取得私鑰的人可以冒用本機的 mTLS 身分（可寫入者還能替換私鑰）",
};

/// 檢查 `path` 的權限；過寬時回傳警告訊息。讀不到檔案資訊（例如檔案不存在）時回傳 `None`。
#[cfg(unix)]
pub fn permission_warning(path: &Path, file: SensitiveFile) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    mode_warning(path, meta.mode(), meta.uid() == 0, file)
}

/// 檢查 `path` 的 ACL；過寬時回傳警告訊息。讀不到安全描述元時回傳 `None`。
#[cfg(windows)]
pub fn permission_warning(path: &Path, file: SensitiveFile) -> Option<String> {
    let finding = windows::read_dacl(path)?;
    acl::acl_warning(path, &finding, file)
}

/// 其他平台：不檢查。
#[cfg(not(any(unix, windows)))]
pub fn permission_warning(_path: &Path, _file: SensitiveFile) -> Option<String> {
    None
}

/// 目錄允許其他使用者存取時的警告訊息（本機緩衝目錄，A5）；沒問題或讀不到時為 `None`。
///
/// 目錄中是本機的監控資料（主機名稱、掛載點、網路介面名稱等），服務帳號必須能寫入，因此規則與
/// 敏感檔案不同：Unix 只看其他使用者（other）的權限（Pulse 自己建立的目錄為 0750）；
/// Windows 看 ACL 是否允許一般使用者存取（同 [`permission_warning`] 的廣泛群組清單）。
#[cfg(unix)]
pub fn dir_permission_warning(path: &Path, what: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    dir_mode_warning(path, mode, what)
}

/// 目錄允許一般使用者存取時的警告訊息（Windows：讀取 DACL）
#[cfg(windows)]
pub fn dir_permission_warning(path: &Path, what: &str) -> Option<String> {
    let finding = windows::read_dacl(path)?;
    acl::dir_acl_warning(path, &finding, what)
}

/// 其他平台：不檢查。
#[cfg(not(any(unix, windows)))]
pub fn dir_permission_warning(_path: &Path, _what: &str) -> Option<String> {
    None
}

/// 目錄的 Unix 權限位元：其他使用者有任何權限時警告（與平台無關，方便測試用語）
#[cfg_attr(not(unix), allow(dead_code))]
fn dir_mode_warning(path: &Path, mode: u32, what: &str) -> Option<String> {
    let mode = mode & 0o7777;
    (mode & 0o007 != 0).then(|| {
        format!(
            "{what} {} 的權限為 {mode:04o}，其他使用者可以存取（其中是本機的監控資料，含主機名稱、掛載點與網路介面名稱）；\
             建議執行 chmod 750 {}",
            path.display(),
            path.display()
        )
    })
}

/// 依 Unix 權限位元與擁有者產生警告（與平台無關，方便在任何平台測試用語）。
///
/// `owner_is_root` 為 true 時允許群組可讀（`root:<服務群組> 640`）。
#[cfg_attr(not(unix), allow(dead_code))]
fn mode_warning(
    path: &Path,
    mode: u32,
    owner_is_root: bool,
    file: SensitiveFile,
) -> Option<String> {
    let mode = mode & 0o777;
    let allowed_group = if owner_is_root { 0o040 } else { 0 };
    let excess = mode & 0o077 & !allowed_group;
    if excess == 0 {
        return None;
    }
    let mut who = Vec::new();
    if excess & 0o070 != 0 {
        who.push("群組");
    }
    if excess & 0o007 != 0 {
        who.push("其他使用者");
    }
    let mut granted = Vec::new();
    if excess & 0o044 != 0 {
        granted.push("讀取");
    }
    if excess & 0o022 != 0 {
        granted.push("寫入");
    }
    if excess & 0o011 != 0 {
        granted.push("執行");
    }
    Some(format!(
        "{} {} 的權限為 {mode:o}，{}可{}；{}。建議執行 chmod 600 {}\
         （或擁有者為 root、群組為服務帳號的群組時 chmod 640，讓服務帳號只能讀不能改）",
        file.name,
        path.display(),
        who.join("與"),
        granted.join("、"),
        file.risk,
        path.display()
    ))
}

// ---- Windows ACL ------------------------------------------------------------

/// DACL 的解析與警告文字（與平台無關，方便在任何平台測試；實際讀取 ACL 只在 Windows）。
#[cfg_attr(not(windows), allow(dead_code))]
mod acl {
    use std::path::Path;

    use super::SensitiveFile;

    /// DACL 的檢查結果。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum DaclFinding {
        /// DACL 為 NULL：任何人都有完全控制
        Null,
        /// 授權給廣泛群組的允許項目（依出現順序，已合併同一群組）
        Grants(Vec<BroadGrant>),
        /// ACL 結構無法解析（不應發生；保守起見不警告，只記 debug）
        Unparsable,
    }

    /// 授權給廣泛群組的一筆允許項目。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct BroadGrant {
        /// 群組的顯示名稱（例如 `BUILTIN\Users`）
        pub(super) principal: &'static str,
        pub(super) read: bool,
        pub(super) write: bool,
    }

    /// ACE 類型：ACCESS_ALLOWED_ACE_TYPE、ACCESS_ALLOWED_CALLBACK_ACE_TYPE（winnt.h）。
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x0;
    const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 0x9;
    /// ACE 旗標：只用於繼承、不套用在物件本身。
    pub(super) const INHERIT_ONLY_ACE: u8 = 0x08;

    /// 讀取權限：FILE_READ_DATA、GENERIC_READ、GENERIC_ALL。
    const READ_MASK: u32 = 0x0000_0001 | 0x8000_0000 | 0x1000_0000;
    /// 寫入權限：FILE_WRITE_DATA、FILE_APPEND_DATA、WRITE_DAC、WRITE_OWNER、GENERIC_WRITE、GENERIC_ALL。
    const WRITE_MASK: u32 =
        0x0000_0002 | 0x0000_0004 | 0x0004_0000 | 0x0008_0000 | 0x4000_0000 | 0x1000_0000;

    /// 依 DACL 檢查結果產生警告。
    pub(super) fn acl_warning(
        path: &Path,
        finding: &DaclFinding,
        file: SensitiveFile,
    ) -> Option<String> {
        let detail = finding_detail(path, finding)?;
        Some(format!(
            "{} {} 的{detail}；{}。建議放在只有執行帳號、SYSTEM 與 Administrators 可存取的目錄，\
             或以 icacls \"{}\" /inheritance:r /grant:r \"<執行帳號>:R\" \"*S-1-5-18:F\" \"*S-1-5-32-544:F\" \
             移除繼承並只授權給這些帳號",
            file.name,
            path.display(),
            file.risk,
            path.display()
        ))
    }

    /// 目錄（本機緩衝）的 DACL 檢查結果產生警告；執行帳號需要修改權限，子項目繼承
    pub(super) fn dir_acl_warning(
        path: &Path,
        finding: &DaclFinding,
        what: &str,
    ) -> Option<String> {
        let detail = finding_detail(path, finding)?;
        Some(format!(
            "{what} {} 的{detail}（其中是本機的監控資料，含主機名稱、掛載點與網路介面名稱）。\
             建議以 icacls \"{}\" /inheritance:r /grant:r \"<執行帳號>:(OI)(CI)M\" \
             \"*S-1-5-18:(OI)(CI)F\" \"*S-1-5-32-544:(OI)(CI)F\" 移除繼承並只授權給執行帳號、SYSTEM 與 Administrators",
            path.display(),
            path.display()
        ))
    }

    /// DACL 檢查結果的說明文字；沒有問題時為 `None`
    fn finding_detail(path: &Path, finding: &DaclFinding) -> Option<String> {
        Some(match finding {
            DaclFinding::Null => "沒有 DACL（NULL DACL），任何人都有完全控制".to_string(),
            DaclFinding::Grants(grants) if !grants.is_empty() => {
                let items: Vec<String> = grants
                    .iter()
                    .map(|g| {
                        let access = match (g.read, g.write) {
                            (true, true) => "讀取、寫入",
                            (true, false) => "讀取",
                            _ => "寫入",
                        };
                        format!("{} 可{access}", g.principal)
                    })
                    .collect();
                format!("ACL 允許一般使用者存取（{}）", items.join("、"))
            }
            DaclFinding::Grants(_) => return None,
            DaclFinding::Unparsable => {
                log::debug!("無法解析 {} 的 ACL，略過權限檢查", path.display());
                return None;
            }
        })
    }

    /// 解析 ACL 的二進位結構（winnt.h 的 `ACL` 標頭與其後的 ACE），找出授權給廣泛群組的允許項目。
    ///
    /// ```text
    /// ACL: AclRevision u8, Sbz1 u8, AclSize u16, AceCount u16, Sbz2 u16
    /// ACE: AceType u8, AceFlags u8, AceSize u16, Mask u32, SID...
    /// SID: Revision u8, SubAuthorityCount u8, IdentifierAuthority [u8; 6]（big endian）,
    ///      SubAuthority [u32; n]（little endian）
    /// ```
    ///
    /// 結構不一致（長度不足、ACE 大小不合理）時回傳 `DaclFinding::Unparsable`，不 panic。
    pub(super) fn parse_acl(acl: &[u8]) -> DaclFinding {
        let u16_at = |b: &[u8], i: usize| -> Option<u16> {
            Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
        };
        let u32_at = |b: &[u8], i: usize| -> Option<u32> {
            Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
        };
        let parse = || -> Option<Vec<BroadGrant>> {
            let size = usize::from(u16_at(acl, 2)?);
            let count = u16_at(acl, 4)?;
            let acl = acl.get(..size)?;
            let mut grants: Vec<BroadGrant> = Vec::new();
            let mut offset = 8usize;
            for _ in 0..count {
                let ace_type = *acl.get(offset)?;
                let flags = *acl.get(offset + 1)?;
                let ace_size = usize::from(u16_at(acl, offset + 2)?);
                if ace_size < 4 {
                    return None;
                }
                let ace = acl.get(offset..offset.checked_add(ace_size)?)?;
                offset += ace_size;
                if !matches!(
                    ace_type,
                    ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
                ) || flags & INHERIT_ONLY_ACE != 0
                {
                    continue;
                }
                let mask = u32_at(ace, 4)?;
                let read = mask & READ_MASK != 0;
                let write = mask & WRITE_MASK != 0;
                if !(read || write) {
                    continue;
                }
                let Some(principal) = broad_principal(ace.get(8..)?) else {
                    continue;
                };
                match grants.iter_mut().find(|g| g.principal == principal) {
                    Some(g) => {
                        g.read |= read;
                        g.write |= write;
                    }
                    None => grants.push(BroadGrant {
                        principal,
                        read,
                        write,
                    }),
                }
            }
            Some(grants)
        };
        parse().map_or(DaclFinding::Unparsable, DaclFinding::Grants)
    }

    /// SID 若為「一般使用者」這類廣泛群組，回傳顯示名稱。
    pub(super) fn broad_principal(sid: &[u8]) -> Option<&'static str> {
        let (&revision, rest) = sid.split_first()?;
        let (&count, rest) = rest.split_first()?;
        if revision != 1 {
            return None;
        }
        let authority = rest
            .get(..6)?
            .iter()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        let (subs, _) = rest.get(6..6 + 4 * usize::from(count))?.as_chunks::<4>();
        let subs: Vec<u32> = subs.iter().map(|c| u32::from_le_bytes(*c)).collect();
        Some(match (authority, subs.as_slice()) {
            (1, [0]) => "Everyone",
            (5, [4]) => "INTERACTIVE",
            (5, [7]) => "ANONYMOUS LOGON",
            (5, [11]) => "Authenticated Users",
            (5, [113]) => "本機帳戶（Local account）",
            (5, [32, 545]) => r"BUILTIN\Users",
            (5, [32, 546]) => r"BUILTIN\Guests",
            (5, [21, _, _, _, 513]) => "Domain Users",
            (5, [21, _, _, _, 514]) => "Domain Guests",
            _ => return None,
        })
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;

    use super::acl::{DaclFinding, parse_acl};

    /// `SE_OBJECT_TYPE::SE_FILE_OBJECT`（accctrl.h）
    const SE_FILE_OBJECT: i32 = 1;
    /// `DACL_SECURITY_INFORMATION`（winnt.h）
    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    const ERROR_SUCCESS: u32 = 0;

    #[link(name = "advapi32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn GetNamedSecurityInfoW(
            object_name: *const u16,
            object_type: i32,
            security_info: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            security_descriptor: *mut *mut c_void,
        ) -> u32;
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    /// 呼叫結束時以 `LocalFree` 釋放安全描述元。
    struct LocalBuffer(*mut c_void);

    impl Drop for LocalBuffer {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 指標由 GetNamedSecurityInfoW 以 LocalAlloc 配置，只釋放一次
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    /// 讀取檔案的 DACL 並解析；API 失敗（例如檔案不存在、沒有 READ_CONTROL 權限）時回傳 `None`。
    pub(super) fn read_dacl(path: &Path) -> Option<DaclFinding> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut dacl: *mut c_void = std::ptr::null_mut();
        let mut sd: *mut c_void = std::ptr::null_mut();
        // SAFETY: wide 以 NUL 結尾；輸出指標都指向本函式的區域變數；
        // 不需要的輸出（owner、group、sacl）傳 NULL，API 允許
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        let _guard = LocalBuffer(sd);
        if status != ERROR_SUCCESS {
            log::debug!("無法讀取 {} 的 ACL（錯誤碼 {status}）", path.display());
            return None;
        }
        if dacl.is_null() {
            return Some(DaclFinding::Null);
        }
        // SAFETY: dacl 指向 sd 內的 ACL 結構（sd 在 _guard 釋放前有效）；
        // 先讀標頭的 AclSize（offset 2，可能未對齊），再以該大小建立切片，
        // 整個 ACL 都在 API 配置的安全描述元內
        let acl = unsafe {
            let size = std::ptr::read_unaligned(dacl.cast::<u8>().add(2).cast::<u16>());
            std::slice::from_raw_parts(dacl.cast::<u8>(), usize::from(size))
        };
        Some(parse_acl(acl))
    }
}

#[cfg(test)]
mod tests {
    use super::acl::*;
    use super::*;

    const KEY: SensitiveFile = CLIENT_KEY_FILE;

    #[test]
    fn buffer_directory_warnings() {
        let p = Path::new("/opt/pulse/buffer");
        for mode in [0o750, 0o700, 0o40750, 0o770] {
            assert_eq!(dir_mode_warning(p, mode, "本機緩衝目錄"), None, "{mode:o}");
        }
        let w = dir_mode_warning(p, 0o40755, "本機緩衝目錄").unwrap();
        assert!(
            w.starts_with("本機緩衝目錄 /opt/pulse/buffer 的權限為 0755")
                && w.contains("chmod 750 /opt/pulse/buffer"),
            "{w}"
        );
        // Windows：Users 可讀時警告，建議授權執行帳號修改（子項目繼承）
        let finding = DaclFinding::Grants(vec![BroadGrant {
            principal: r"BUILTIN\Users",
            read: true,
            write: false,
        }]);
        let w = dir_acl_warning(Path::new(r"C:\pulse\buffer"), &finding, "本機緩衝目錄").unwrap();
        assert!(
            w.contains(r"BUILTIN\Users 可讀取") && w.contains("(OI)(CI)M"),
            "{w}"
        );
        assert_eq!(
            dir_acl_warning(Path::new("x"), &DaclFinding::Grants(vec![]), "d"),
            None
        );
    }

    #[test]
    fn mode_owner_only_is_fine() {
        let p = Path::new("client.key");
        for mode in [0o600, 0o400, 0o100_600, 0o700] {
            assert_eq!(mode_warning(p, mode, false, KEY), None, "{mode:o}");
            assert_eq!(mode_warning(p, mode, true, KEY), None, "{mode:o}");
        }
    }

    #[test]
    fn mode_group_read_allowed_only_when_owned_by_root() {
        // S8：root:pulse 640 / 440 是建議配置，不警告
        let p = Path::new("client.key");
        assert_eq!(mode_warning(p, 0o640, true, KEY), None);
        assert_eq!(mode_warning(p, 0o440, true, KEY), None);
        // 擁有者不是 root（例如 pulse:pulse 640）：同群組的其他帳號也能讀
        let w = mode_warning(p, 0o640, false, KEY).unwrap();
        assert!(w.contains("640") && w.contains("群組可讀取；"), "{w}");
        // 即使擁有者是 root，群組可寫入或其他使用者可讀仍警告
        let w = mode_warning(p, 0o660, true, KEY).unwrap();
        assert!(w.contains("群組可寫入；") && !w.contains("讀取"), "{w}");
        let w = mode_warning(p, 0o644, true, KEY).unwrap();
        assert!(w.contains("其他使用者可讀取；"), "{w}");
        assert!(!w.contains("群組與"), "群組可讀在 root 擁有時不算：{w}");
    }

    #[test]
    fn mode_warning_text() {
        let p = Path::new("client.key");
        let w = mode_warning(p, 0o666, false, KEY).unwrap();
        assert!(
            w.starts_with("客戶端私鑰 server.client_key client.key 的權限為 666，群組與其他使用者可讀取、寫入；")
                && w.contains("chmod 600 client.key")
                && w.contains("chmod 640")
                && w.contains("mTLS 身分"),
            "{w}"
        );
        let w = mode_warning(p, 0o601, false, AGENT_ID_FILE).unwrap();
        assert!(
            w.contains("其他使用者可執行；") && w.contains("agent_id 等同身分憑據"),
            "{w}"
        );
    }

    // ---- ACL 解析（與平台無關）----

    fn sid(authority: u8, subs: &[u32]) -> Vec<u8> {
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

    fn ace(ace_type: u8, flags: u8, mask: u32, sid: &[u8]) -> Vec<u8> {
        let size = u16::try_from(8 + sid.len()).unwrap();
        let mut v = vec![ace_type, flags];
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&mask.to_le_bytes());
        v.extend_from_slice(sid);
        v
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

    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    const FILE_GENERIC_READ: u32 = 0x0012_0089;
    const READ_CONTROL_ONLY: u32 = 0x0002_0000;

    #[test]
    fn acl_owner_system_admins_only_is_fine() {
        let a = acl(&[
            ace(0, 0x10, FILE_ALL_ACCESS, &sid(5, &[18])), // SYSTEM（繼承）
            ace(0, 0x10, FILE_ALL_ACCESS, &sid(5, &[32, 544])), // Administrators
            ace(0, 0, FILE_ALL_ACCESS, &sid(5, &[21, 1, 2, 3, 1001])), // 某個使用者
        ]);
        assert_eq!(parse_acl(&a), DaclFinding::Grants(Vec::new()));
        assert_eq!(acl_warning(Path::new("k"), &parse_acl(&a), KEY), None);
    }

    #[test]
    fn acl_broad_groups_are_reported() {
        let a = acl(&[
            ace(0, 0x10, FILE_ALL_ACCESS, &sid(5, &[18])),
            ace(0, 0x10, FILE_GENERIC_READ, &sid(5, &[32, 545])), // Users 可讀（繼承自 C:\）
            ace(0, 0x10, 0x0013_01BF, &sid(5, &[11])),            // Authenticated Users 修改
            ace(0, 0, 0x8000_0000, &sid(1, &[0])),                // Everyone GENERIC_READ
            ace(0, 0, 0x0000_0002, &sid(5, &[32, 545])),          // Users 另一筆：寫入
            ace(0, 0, FILE_GENERIC_READ, &sid(5, &[21, 9, 8, 7, 513])), // Domain Users
        ]);
        let finding = parse_acl(&a);
        assert_eq!(
            finding,
            DaclFinding::Grants(vec![
                BroadGrant {
                    principal: r"BUILTIN\Users",
                    read: true,
                    write: true
                },
                BroadGrant {
                    principal: "Authenticated Users",
                    read: true,
                    write: true
                },
                BroadGrant {
                    principal: "Everyone",
                    read: true,
                    write: false
                },
                BroadGrant {
                    principal: "Domain Users",
                    read: true,
                    write: false
                },
            ])
        );
        let w = acl_warning(Path::new(r"C:\pulse\client.key"), &finding, KEY).unwrap();
        assert!(
            w.contains(r"BUILTIN\Users 可讀取、寫入")
                && w.contains("Everyone 可讀取")
                && w.contains("icacls")
                && w.contains("mTLS 身分"),
            "{w}"
        );
    }

    #[test]
    fn acl_ignores_deny_inherit_only_and_unrelated_rights() {
        let a = acl(&[
            ace(1, 0, FILE_ALL_ACCESS, &sid(1, &[0])), // 拒絕項目：不計
            ace(
                0,
                INHERIT_ONLY_ACE | 0x03,
                FILE_ALL_ACCESS,
                &sid(5, &[32, 545]),
            ), // 只用於繼承
            ace(0, 0, READ_CONTROL_ONLY, &sid(5, &[11])), // 只能讀 ACL，不能讀內容
            ace(0, 0, FILE_GENERIC_READ, &sid(5, &[32, 547])), // Power Users：不在清單
        ]);
        assert_eq!(parse_acl(&a), DaclFinding::Grants(Vec::new()));
    }

    #[test]
    fn null_dacl_and_malformed_acl() {
        let w = acl_warning(Path::new("agent.id"), &DaclFinding::Null, AGENT_ID_FILE).unwrap();
        assert!(w.contains("NULL DACL") && w.contains("agent_id"), "{w}");

        // 結構不一致時不 panic、不警告
        let good = acl(&[ace(0, 0, FILE_GENERIC_READ, &sid(1, &[0]))]);
        for bad in [
            Vec::new(),
            good[..7].to_vec(),
            good[..good.len() - 1].to_vec(), // AclSize 超出實際長度
            {
                let mut b = good.clone();
                b[10] = 2; // AceSize < 4
                b[11] = 0;
                b
            },
            {
                let mut b = good.clone();
                b[4] = 5; // AceCount 多於實際
                b
            },
        ] {
            assert_eq!(parse_acl(&bad), DaclFinding::Unparsable, "{bad:?}");
            assert_eq!(
                acl_warning(Path::new("x"), &DaclFinding::Unparsable, KEY),
                None
            );
        }
        // SID 截斷 → 不是廣泛群組，但不 panic
        assert_eq!(broad_principal(&[1, 2, 0, 0, 0, 0, 0, 5, 32, 0]), None);
        assert_eq!(broad_principal(&sid(5, &[32, 545])), Some(r"BUILTIN\Users"));
        assert_eq!(broad_principal(&[2, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]), None);
    }

    /// 測試用暫存檔（位於 TMP，不寫進 repo）。
    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("pulse-file-perms-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, "secret").unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn unix_real_file_modes() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let path = temp_file("client.key");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(permission_warning(&path, KEY), None);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let w = permission_warning(&path, KEY).unwrap();
        assert!(w.contains("644") && w.contains("其他使用者可讀取"), "{w}");
        // 640：只有擁有者是 root 時才允許（Docker 中以 root 執行測試時擁有者就是 root）
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let owner_is_root = std::fs::metadata(&path).unwrap().uid() == 0;
        assert_eq!(permission_warning(&path, KEY).is_none(), owner_is_root);
        // 不存在的檔案 → None
        assert_eq!(permission_warning(&path.with_extension("nope"), KEY), None);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(windows)]
    fn icacls(path: &Path, args: &[&str]) {
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "icacls {args:?}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_real_file_acl() {
        let path = temp_file("client.key");
        // 只授權給 SYSTEM、Administrators 與擁有者（OWNER RIGHTS 以外的明確項目）
        icacls(
            &path,
            &[
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:F",
                "/grant:r",
                "*S-1-5-32-544:F",
                "/grant:r",
                "*S-1-3-4:F", // OWNER RIGHTS：讓測試之後仍能修改 ACL 與刪除
            ],
        );
        assert_eq!(permission_warning(&path, KEY), None);

        // 加上 Users 可讀 → 警告
        icacls(&path, &["/grant", "*S-1-5-32-545:R"]);
        let w = permission_warning(&path, KEY).unwrap();
        assert!(
            w.contains(r"BUILTIN\Users 可讀取") && w.contains("icacls"),
            "{w}"
        );

        // Everyone 可修改 → 一併列出
        icacls(&path, &["/grant", "*S-1-1-0:M"]);
        let w = permission_warning(&path, KEY).unwrap();
        assert!(w.contains("Everyone 可讀取、寫入"), "{w}");

        // 不存在的檔案 → None
        assert_eq!(permission_warning(&path.with_extension("nope"), KEY), None);

        icacls(&path, &["/reset"]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
