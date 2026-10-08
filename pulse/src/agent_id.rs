//! Agent 識別：第一次啟動時產生 UUID v4 並寫入檔案，之後讀檔沿用。
//!
//! - 讀檔時去除 UTF-8 BOM 與前後空白，並要求為 UUID 文字格式（8-4-4-4-12 十六進位，共 36 字元）。
//!   格式不合法時回傳錯誤讓程式結束，**不會**默默換新 ID（避免伺服器端身分改變）。
//!   檔案超過 `MAX_ID_FILE_SIZE` 時不讀完整個檔案，直接報錯。
//! - 識別不分大小寫：讀入後一律轉為小寫（與伺服器的正規化一致），檔案本身不改寫。
//! - 未啟用 mTLS 時 agent_id 就是身分憑據：Unix 上新建的檔案權限為 0600（僅擁有者可讀寫），
//!   既有檔案若可被其他使用者讀取會記 warn。Windows 上權限繼承自所在目錄。
//! - 寫檔不會覆蓋既有檔案：先寫入暫存檔並 sync，再以 hard link 發佈（目的檔已存在則失敗）；
//!   檔案系統不支援 hard link 時退回以 create_new 直接建立。
//!   兩個實例同時首次啟動時，後到者會改讀先到者的檔案，兩者使用同一個 ID。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// UTF-8 BOM（Windows 記事本存檔時可能加上）。
const UTF8_BOM: char = '\u{feff}';

/// 讀到空的 agent_id 檔案（可能是另一個實例尚未寫完）時的重試次數與間隔（合計約 1 秒）。
const EMPTY_FILE_RETRIES: u32 = 20;
const EMPTY_FILE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// agent_id 檔案大小上限（bytes）。合法內容只有 36 字元加上 BOM / 空白，
/// 上限只是避免誤指到大型檔案時把整個檔案讀進記憶體。
const MAX_ID_FILE_SIZE: u64 = 1024;

/// 讀取 `path` 中的 agent_id；檔案不存在時產生新的 UUID v4 並寫入。
///
/// 回傳 `(agent_id, 是否為新產生)`。內含檔案 I/O 與短暫的重試等待，
/// 在 async 環境中請用 `spawn_blocking` 呼叫。
pub fn load_or_create(path: &Path) -> io::Result<(String, bool)> {
    if let Some(id) = read_existing_patiently(path)? {
        return Ok((id, false));
    }

    let id = uuid::Uuid::new_v4().to_string();
    if publish(path, &id)? {
        return Ok((id, true));
    }
    // 另一個實例搶先建立了檔案，改用它的 ID
    let id = read_existing_patiently(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("agent_id 檔案 {} 建立後又消失", path.display()),
        )
    })?;
    Ok((id, false))
}

/// 同 `read_existing`，但檔案存在且內容為空時短暫重試（合計約 1 秒）：
/// 在 create_new 退回路徑下，另一個實例可能剛建立檔案、尚未寫入內容。
/// 非空但不合法的內容立即回報錯誤，不等待。
fn read_existing_patiently(path: &Path) -> io::Result<Option<String>> {
    for _ in 0..EMPTY_FILE_RETRIES {
        match read_trimmed(path)? {
            Some(content) if content.is_empty() => std::thread::sleep(EMPTY_FILE_RETRY_DELAY),
            other => return validate(path, other),
        }
    }
    validate(path, read_trimmed(path)?)
}

/// 讀取並驗證既有檔案（不重試）；檔案不存在時回傳 `Ok(None)`。僅供測試。
#[cfg(test)]
fn read_existing(path: &Path) -> io::Result<Option<String>> {
    validate(path, read_trimmed(path)?)
}

/// 讀檔（最多 `MAX_ID_FILE_SIZE` bytes）並去除 UTF-8 BOM 與前後空白；檔案不存在時回傳 `Ok(None)`。
fn read_trimmed(path: &Path) -> io::Result<Option<String>> {
    let read_err = |e: io::Error| {
        io::Error::new(
            e.kind(),
            format!("無法讀取 agent_id 檔案 {}：{e}", path.display()),
        )
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(read_err(e)),
    };
    // 多讀 1 byte 以判斷是否超過上限
    let mut buf = Vec::new();
    file.take(MAX_ID_FILE_SIZE + 1)
        .read_to_end(&mut buf)
        .map_err(read_err)?;
    if buf.len() as u64 > MAX_ID_FILE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "agent_id 檔案 {} 超過 {MAX_ID_FILE_SIZE} bytes，不是有效的 agent_id 檔案；\
                 請確認 agent.id_file 是否指錯檔案，或刪除此檔案後重新啟動以產生新的 agent_id",
                path.display()
            ),
        ));
    }
    let content = String::from_utf8(buf).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "agent_id 檔案 {} 不是 UTF-8 文字；若要重新產生 agent_id，請刪除此檔案後重新啟動",
                path.display()
            ),
        )
    })?;
    Ok(Some(
        content.trim_start_matches(UTF8_BOM).trim().to_string(),
    ))
}

/// 驗證內容為 UUID 文字格式，並轉為小寫（agent_id 不分大小寫，伺服器以小寫為準）。
fn validate(path: &Path, content: Option<String>) -> io::Result<Option<String>> {
    let Some(id) = content else {
        return Ok(None);
    };
    if !is_uuid_text(&id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "agent_id 檔案 {} 的內容「{}」不是有效的 UUID（需為 8-4-4-4-12 格式的十六進位字元，共 36 字元）。\
                 若要重新產生 agent_id，請刪除此檔案後重新啟動（注意：伺服器會把它視為一台新的主機）",
                path.display(),
                display_snippet(&id)
            ),
        ));
    }
    Ok(Some(id.to_ascii_lowercase()))
}

/// Unix：agent_id 檔案可被群組或其他使用者讀取時，回傳警告訊息（不自動修改權限）。
///
/// 由呼叫端決定輸出方式（一般模式寫日誌；`--print-agent-id` 尚未初始化日誌，改寫 stderr）。
/// 讀不到檔案資訊時回傳 `None`。
#[cfg(unix)]
pub fn permission_warning(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then(|| {
        format!(
            "agent_id 檔案 {} 的權限為 {mode:o}，其他使用者可讀取；未啟用 mTLS 時 agent_id 等同身分憑據，\
             建議執行 chmod 600 {}",
            path.display(),
            path.display()
        )
    })
}

/// 非 Unix：權限由所在目錄的 ACL 決定，不檢查。
#[cfg(not(unix))]
pub fn permission_warning(_path: &Path) -> Option<String> {
    None
}

/// 以原子方式寫入新檔，不覆蓋既有檔案。
///
/// 1. 寫入同目錄的暫存檔並 `sync_all`（避免斷電後留下空檔）
/// 2. 以 hard link 發佈到目的路徑：目的已存在時失敗 → 代表另一個實例搶先建立
/// 3. 檔案系統不支援 hard link（見 `hard_link_unsupported`）時，退回以
///    `create_new` 直接建立目的檔並寫入、`sync_all`；同樣不會覆蓋既有檔案
///
/// 其他錯誤（例如 `PermissionDenied`）直接回報。暫存檔在任何情況下都會刪除。
/// 回傳 `true` 表示由本實例建立；`false` 表示檔案已被其他實例建立。
fn publish(path: &Path, id: &str) -> io::Result<bool> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| with_ctx("建立 agent_id 檔案所在目錄", parent, e))?;
    }
    let content = format!("{id}\n");

    let tmp = temp_path(path);
    let result = write_new_file(&tmp, &content)
        .map_err(|e| with_ctx("寫入暫存檔", &tmp, e))
        .and_then(|()| match fs::hard_link(&tmp, path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) if hard_link_unsupported(&e) => {
                log::debug!("檔案系統不支援 hard link（{e}），改用 create_new 建立 agent_id 檔案");
                publish_with_create_new(path, &content)
            }
            Err(e) => Err(with_ctx("建立 agent_id 檔案", path, e)),
        });
    // 不論成功與否都刪除暫存檔（寫入失敗時可能只寫了一半）；刪除失敗不影響結果
    let _ = fs::remove_file(&tmp);
    result
}

/// 退回路徑：以 `create_new` 直接建立目的檔。已存在 → `Ok(false)`。
/// 建立後寫入失敗時刪除該檔，避免留下空檔或半個 ID。
fn publish_with_create_new(path: &Path, content: &str) -> io::Result<bool> {
    match write_new_file(path, content) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => {
            let _ = fs::remove_file(path);
            Err(with_ctx("寫入 agent_id 檔案", path, e))
        }
    }
}

/// 建立新檔（已存在則以 `AlreadyExists` 失敗）、寫入並 `sync_all`。
///
/// Unix 上以 0600 建立（僅擁有者可讀寫）；暫存檔以 hard link 發佈，目的檔共用同一個 inode，
/// 權限相同。
fn write_new_file(path: &Path, content: &str) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

/// hard link 失敗時，是否代表「檔案系統不支援 hard link」而應退回 `create_new`。
///
/// 判斷依據為 `ErrorKind::Unsupported`，或下列平台特定的作業系統錯誤碼；
/// 其他錯誤（例如權限不足）直接回報。
fn hard_link_unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
        || e.raw_os_error()
            .is_some_and(|code| HARD_LINK_UNSUPPORTED_OS_ERRORS.contains(&code))
}

/// Windows `CreateHardLinkW` 在不支援 hard link 時的錯誤碼（winerror.h）：
/// - `ERROR_INVALID_FUNCTION`（1）：FAT32 / exFAT 等檔案系統不支援此操作
/// - `ERROR_NOT_SUPPORTED`（50）：網路共用或檔案系統驅動程式不支援此要求
#[cfg(windows)]
const HARD_LINK_UNSUPPORTED_OS_ERRORS: &[i32] = &[1, 50];

/// Linux `link(2)` 在不支援 hard link 時的 errno（asm-generic/errno-base.h、errno.h）：
/// - `EPERM`（1）：「The filesystem containing oldpath and newpath does not support the
///   creation of hard links」（vfat / exfat 等）。暫存檔由本程式建立，不會因擁有者不符而觸發
///   protected_hardlinks 的 EPERM。
/// - `EOPNOTSUPP`（95）：部分 FUSE / 網路檔案系統
#[cfg(target_os = "linux")]
const HARD_LINK_UNSUPPORTED_OS_ERRORS: &[i32] = &[1, 95];

/// macOS / iOS `link(2)`（sys/errno.h）：`EPERM`（1）、`ENOTSUP`（45）、`EOPNOTSUPP`（102）
#[cfg(any(target_os = "macos", target_os = "ios"))]
const HARD_LINK_UNSUPPORTED_OS_ERRORS: &[i32] = &[1, 45, 102];

/// 其他平台：只依 `ErrorKind::Unsupported` 判斷
#[cfg(not(any(windows, target_os = "linux", target_os = "macos", target_os = "ios")))]
const HARD_LINK_UNSUPPORTED_OS_ERRORS: &[i32] = &[];

fn with_ctx(what: &str, p: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("無法{what} {}：{e}", p.display()))
}

/// 與目的檔同目錄的唯一暫存檔名（同目錄才能 hard link / rename）。
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "agent.id".into());
    name.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    path.with_file_name(name)
}

/// 是否為 UUID 文字格式：8-4-4-4-12 個十六進位字元（大小寫皆可），共 36 字元。
fn is_uuid_text(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// 錯誤訊息用：過長內容只顯示開頭。
fn display_snippet(s: &str) -> String {
    const LIMIT: usize = 64;
    match s.char_indices().nth(LIMIT) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 測試用暫存目錄（位於 TMP，不寫進 repo），每個測試各自一個。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("pulse-agent-id-tests")
            .join(format!("{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 目錄中只剩下指定的檔案（確認暫存檔已清掉）。
    fn only_file(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn create_then_reuse() {
        let dir = temp_dir("reuse");
        let path = dir.join("sub").join("agent.id");

        let (id1, created1) = load_or_create(&path).unwrap();
        assert!(created1);
        let parsed = uuid::Uuid::parse_str(&id1).unwrap();
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(only_file(&dir.join("sub")), vec!["agent.id"]);

        let (id2, created2) = load_or_create(&path).unwrap();
        assert!(!created2);
        assert_eq!(id1, id2);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bom_and_whitespace_are_stripped() {
        let dir = temp_dir("bom");
        let path = dir.join("agent.id");
        fs::write(&path, "\u{feff}  6F1C0E4A-1b2c-4d3e-9f00-112233445566 \r\n").unwrap();

        let (id, created) = load_or_create(&path).unwrap();
        assert!(!created);
        // X3：讀入後轉為小寫，檔案本身不改寫
        assert_eq!(id, "6f1c0e4a-1b2c-4d3e-9f00-112233445566");
        assert!(fs::read_to_string(&path).unwrap().contains("6F1C0E4A-1b2c"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_or_non_utf8_file_is_rejected() {
        // P11：超過上限不讀完整個檔案，直接報錯
        let dir = temp_dir("oversize");
        let path = dir.join("agent.id");
        let id = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
        // 剛好在上限內（前後空白）仍可接受
        let padded = format!("{id}{}", " ".repeat(MAX_ID_FILE_SIZE as usize - id.len()));
        fs::write(&path, &padded).unwrap();
        assert_eq!(load_or_create(&path).unwrap().0, id);
        // 超過 1 byte → 錯誤，且不會換新 ID
        fs::write(&path, format!("{padded} ")).unwrap();
        let err = load_or_create(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("超過 1024 bytes"), "{err}");
        // 大型檔案
        fs::write(&path, vec![b'a'; 10 * 1024 * 1024]).unwrap();
        assert!(load_or_create(&path).is_err());
        // 非 UTF-8
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let err = load_or_create(&path).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");

        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn new_file_is_owner_only_on_unix() {
        // P3：未啟用 mTLS 時 agent_id 等同身分憑據
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("agent.id");
        load_or_create(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
        // 退回路徑（create_new）同樣是 0600
        let path2 = dir.join("agent2.id");
        assert!(publish_with_create_new(&path2, "11111111-2222-4333-8444-555555555555\n").unwrap());
        let mode = fs::metadata(&path2).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
        assert_eq!(permission_warning(&path2), None);
        // 權限過寬 → 回傳警告（由呼叫端輸出）
        fs::set_permissions(&path2, fs::Permissions::from_mode(0o644)).unwrap();
        let warning = permission_warning(&path2).unwrap();
        assert!(
            warning.contains("644") && warning.contains("chmod 600"),
            "{warning}"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_content_is_an_error_not_regenerated() {
        let dir = temp_dir("invalid");
        let path = dir.join("agent.id");
        for bad in [
            "",
            " \n",
            "my-agent-01",
            "6f1c0e4a1b2c4d3e9f00112233445566",       // 無連字號
            "{6f1c0e4a-1b2c-4d3e-9f00-112233445566}", // 大括號
            "6f1c0e4a-1b2c-4d3e-9f00-11223344556g",   // 非十六進位
            "6f1c0e4a-1b2c4-d3e-9f00-112233445566",   // 連字號位置錯
        ] {
            fs::write(&path, bad).unwrap();
            let err = load_or_create(&path).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{bad:?}");
            assert!(err.to_string().contains("請刪除此檔案"), "{err}");
            // 檔案內容不得被改寫
            assert_eq!(fs::read_to_string(&path).unwrap(), bad);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publish_does_not_overwrite_existing() {
        let dir = temp_dir("race");
        let path = dir.join("agent.id");
        let winner = "11111111-2222-4333-8444-555555555555";
        fs::write(&path, format!("{winner}\n")).unwrap();

        // 模擬另一個實例在本實例讀檔之後、寫檔之前搶先建立
        assert!(!publish(&path, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap());
        assert_eq!(read_existing(&path).unwrap().as_deref(), Some(winner));
        assert_eq!(only_file(&dir), vec!["agent.id"]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_first_start_agrees_on_one_id() {
        let dir = temp_dir("concurrent");
        let path = dir.join("agent.id");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || load_or_create(&path).unwrap())
            })
            .collect();
        let results: Vec<(String, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = &results[0].0;
        assert!(results.iter().all(|(id, _)| id == first));
        assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1);
        assert_eq!(only_file(&dir), vec!["agent.id"]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fallback_decision_by_error_kind() {
        assert!(hard_link_unsupported(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        // 沒有 OS 錯誤碼的其他錯誤直接回報，不退回
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::NotFound,
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::Other,
        ] {
            assert!(!hard_link_unsupported(&io::Error::from(kind)), "{kind:?}");
        }
        // 無法辨識的 OS 錯誤碼不退回
        assert!(!hard_link_unsupported(&io::Error::from_raw_os_error(
            0x7fff_fff0
        )));
    }

    #[cfg(windows)]
    #[test]
    fn windows_not_supported_codes_fall_back() {
        // ERROR_INVALID_FUNCTION(1)、ERROR_NOT_SUPPORTED(50)
        for code in [1, 50] {
            assert!(
                hard_link_unsupported(&io::Error::from_raw_os_error(code)),
                "code {code}"
            );
        }
        // ERROR_ACCESS_DENIED(5)、ERROR_ALREADY_EXISTS(183)、ERROR_DISK_FULL(112) 不退回
        for code in [5, 183, 112] {
            assert!(
                !hard_link_unsupported(&io::Error::from_raw_os_error(code)),
                "code {code}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_not_supported_codes_fall_back() {
        // EPERM(1)、EOPNOTSUPP(95) 退回；EACCES(13)、EEXIST(17)、ENOSPC(28) 不退回
        for code in [1, 95] {
            assert!(hard_link_unsupported(&io::Error::from_raw_os_error(code)));
        }
        for code in [13, 17, 28] {
            assert!(!hard_link_unsupported(&io::Error::from_raw_os_error(code)));
        }
    }

    #[test]
    fn create_new_fallback_never_overwrites() {
        let dir = temp_dir("fallback");
        let path = dir.join("agent.id");
        let id = "11111111-2222-4333-8444-555555555555";

        // 首次：建立並寫入完整內容
        assert!(publish_with_create_new(&path, &format!("{id}\n")).unwrap());
        assert_eq!(read_existing(&path).unwrap().as_deref(), Some(id));

        // 已存在：回傳 false，內容不變
        assert!(!publish_with_create_new(&path, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\n").unwrap());
        assert_eq!(read_existing(&path).unwrap().as_deref(), Some(id));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publish_error_is_reported_and_temp_cleaned() {
        let dir = temp_dir("publish-err");
        // 父路徑是一個檔案 → 無法建立目錄，應回報錯誤
        let blocker = dir.join("not-a-dir");
        fs::write(&blocker, "x").unwrap();
        let err = publish(
            &blocker.join("agent.id"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not-a-dir"), "{err}");
        assert_eq!(only_file(&dir), vec!["not-a-dir"]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loser_waits_for_partially_written_file() {
        let dir = temp_dir("loser");
        let path = dir.join("agent.id");
        // 模擬另一個實例以 create_new 建立了空檔，稍後才寫入內容
        fs::write(&path, "").unwrap();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            fs::write(writer_path, "11111111-2222-4333-8444-555555555555\n").unwrap();
        });
        // 讀到空檔時應等待對方寫完，取得對方的 ID，而不是報錯或覆蓋
        let (id, created) = load_or_create(&path).unwrap();
        assert!(!created);
        assert_eq!(id, "11111111-2222-4333-8444-555555555555");
        writer.join().unwrap();

        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(not(unix))]
    #[test]
    fn permission_warning_is_unix_only() {
        let dir = temp_dir("perm");
        let path = dir.join("agent.id");
        load_or_create(&path).unwrap();
        assert_eq!(permission_warning(&path), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn uuid_text_format() {
        assert!(is_uuid_text("6f1c0e4a-1b2c-4d3e-9f00-112233445566"));
        assert!(is_uuid_text("6F1C0E4A-1B2C-4D3E-9F00-112233445566"));
        assert!(!is_uuid_text("6f1c0e4a-1b2c-4d3e-9f00-1122334455667"));
        assert!(!is_uuid_text("６f1c0e4a-1b2c-4d3e-9f00-11223344556"));
    }
}
