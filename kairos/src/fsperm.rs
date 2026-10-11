//! 檔案與目錄權限（審計 S3），以及伺服器私鑰的權限檢查（審計 S8）。
//!
//! Unix：Kairos 寫出的 report JSONL、`agents.json` 與日誌檔最多 0640（擁有者讀寫、group 唯讀、
//! 其他人無權限），自行建立的目錄為 0750，不依賴行程的 umask（手動執行時 umask 常為 022）。
//! 既有的檔案在開啟時收緊到最多 0640（只移除權限，不會放寬，例如 0600 維持 0600）。
//!
//! Windows 沒有對應的 mode：檔案繼承所在目錄的 ACL，請依 README「檔案權限」設定目錄的 ACL。
//! 這裡的函式在 Windows 上只建立目錄或什麼都不做。

use std::io;
use std::path::Path;

/// 資料檔與日誌檔的權限上限（Unix）
#[cfg_attr(not(unix), allow(dead_code))]
pub const FILE_MODE: u32 = 0o640;

/// Kairos 建立的目錄的權限（Unix）
#[cfg_attr(not(unix), allow(dead_code))]
pub const DIR_MODE: u32 = 0o750;

/// 收緊後的權限：只保留 [`FILE_MODE`] 允許的位元
#[cfg_attr(not(unix), allow(dead_code))]
pub fn restricted(mode: u32) -> u32 {
    mode & FILE_MODE
}

/// 建立目錄（含上層目錄）；Unix 上新建立的目錄為 0750，既有的目錄不變
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(path)
}

/// [`create_dir_all`] 的非同步版本
pub async fn create_dir_all_async(path: &Path) -> io::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(DIR_MODE);
    builder.create(path).await
}

/// 讓開啟選項在建立新檔時使用 0640（Unix；實際權限仍受 umask 收緊）
pub fn private_std_options(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    options
}

/// [`private_std_options`] 的 tokio 版本
pub fn private_tokio_options(options: &mut tokio::fs::OpenOptions) -> &mut tokio::fs::OpenOptions {
    #[cfg(unix)]
    options.mode(FILE_MODE);
    options
}

/// 把已開啟的檔案收緊到最多 0640（Unix）；回傳是否有變更。Windows 上不做任何事
pub fn restrict_open_file(file: &std::fs::File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode() & 0o7777;
        if restricted(mode) != mode {
            file.set_permissions(std::fs::Permissions::from_mode(restricted(mode)))?;
            return Ok(true);
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(false)
}

/// [`restrict_open_file`] 的 tokio 版本
pub async fn restrict_open_tokio_file(file: &tokio::fs::File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata().await?.permissions().mode() & 0o7777;
        if restricted(mode) != mode {
            file.set_permissions(std::fs::Permissions::from_mode(restricted(mode)))
                .await?;
            return Ok(true);
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(false)
}

/// 把路徑上的檔案收緊到最多 0640（Unix；不跟隨符號連結以外的特殊處理，與 `chmod` 相同）
pub fn restrict_path(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o7777;
        if restricted(mode) != mode {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(restricted(mode)))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// 既有目錄允許其他使用者（other）存取時的警告訊息（Unix）；沒問題、讀不到或非 Unix 時為 `None`。
/// Kairos 不會自行改變既有目錄的權限（可能是管理者刻意的設定），只提醒
pub fn dir_permission_warning(path: &Path, what: &str) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o7777;
        (mode & 0o007 != 0).then(|| {
            format!(
                "{} {} 的權限為 {:04o}，其他使用者可以存取（其中的檔案含 agent 的 IP 與主機名稱）；建議執行 chmod 750 {}",
                what,
                path.display(),
                mode,
                path.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (path, what);
        None
    }
}

/// 從 `/proc/self/status` 的 `Gid:` 行取得 effective GID（第二個欄位）
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_effective_gid(status: &str) -> Option<u32> {
    let line = status.lines().find(|l| l.starts_with("Gid:"))?;
    line.split_whitespace().nth(2)?.parse().ok()
}

/// 行程的 effective GID：只在 Linux 讀取 `/proc/self/status`（不需 unsafe 或額外依賴）；其他平台為 `None`
#[cfg(unix)]
fn effective_gid() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        parse_effective_gid(&std::fs::read_to_string("/proc/self/status").ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 私鑰權限中過寬的部分（Unix 權限位元與檔案 GID）。`own_gid` 為行程的 effective GID（無法得知時為 `None`）。
///
/// - other 有任何權限、group 可寫入或執行：過寬；
/// - group 可讀：只在檔案的群組就是 Kairos 執行時的群組（INSTALL 建議的 `root:kairos 640`）時允許；
///   無法得知行程的群組時（非 Linux）不視為過寬。
#[cfg_attr(not(unix), allow(dead_code))]
pub fn key_mode_problems(mode: u32, file_gid: u32, own_gid: Option<u32>) -> Vec<String> {
    let mut problems = Vec::new();
    if mode & 0o007 != 0 {
        problems.push("其他使用者（other）有權限".to_string());
    }
    if mode & 0o030 != 0 {
        problems.push("group 可寫入或執行".to_string());
    }
    if mode & 0o040 != 0
        && let Some(own) = own_gid
        && own != file_gid
    {
        problems.push(format!(
            "group 可讀取，但檔案的群組（GID {}）不是 Kairos 執行時的群組（GID {}）",
            file_gid, own
        ));
    }
    problems
}

/// 伺服器私鑰權限過寬時的警告訊息（Unix，S8）；沒問題、讀不到（建立 TLS 設定時會另外報錯）或非 Unix 時為 `None`
pub fn key_permission_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(path).ok()?;
        let mode = meta.permissions().mode() & 0o7777;
        let problems = key_mode_problems(mode, meta.gid(), effective_gid());
        (!problems.is_empty()).then(|| {
            format!(
                "伺服器私鑰 {} 的權限過寬（{:04o}：{}），服務帳號以外的帳號可能讀取或修改私鑰；建議執行 chown root:kairos {} && chmod 640 {}（群組換成執行 Kairos 的群組）",
                path.display(),
                mode,
                problems.join("；"),
                path.display(),
                path.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_gid_is_parsed_from_proc_status() {
        let status =
            "Name:\tkairos\nUid:\t998\t998\t998\t998\nGid:\t997\t996\t997\t997\nGroups:\t997\n";
        assert_eq!(parse_effective_gid(status), Some(996));
        assert_eq!(parse_effective_gid("Name:\tx\n"), None);
    }

    #[test]
    fn key_mode_problems_allow_only_owner_and_own_group_read() {
        // S8：600、400、以及群組是自己的 640／440 都不警告
        for mode in [0o600, 0o400, 0o640, 0o440] {
            assert!(
                key_mode_problems(mode, 100, Some(100)).is_empty(),
                "{mode:o}"
            );
        }
        // 無法得知行程的群組時，group 可讀不視為過寬
        assert!(key_mode_problems(0o640, 100, None).is_empty());
        // group 可讀但不是自己的群組
        let p = key_mode_problems(0o640, 0, Some(100));
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("GID 0"), "{p:?}");
        // other 有權限、group 可寫
        assert_eq!(key_mode_problems(0o644, 100, Some(100)).len(), 1);
        assert_eq!(key_mode_problems(0o660, 100, Some(100)).len(), 1);
        assert_eq!(key_mode_problems(0o666, 0, Some(100)).len(), 3);
    }

    #[test]
    fn restricted_only_removes_bits() {
        assert_eq!(restricted(0o644), 0o640);
        assert_eq!(restricted(0o666), 0o640);
        assert_eq!(restricted(0o777), 0o640);
        assert_eq!(restricted(0o640), 0o640);
        // 不會放寬
        assert_eq!(restricted(0o600), 0o600);
        assert_eq!(restricted(0o400), 0o400);
    }

    #[cfg(unix)]
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kairos-fsperm-{}-{}-{}",
            tag,
            std::process::id(),
            crate::registry::next_conn_id()
        ))
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    #[test]
    fn new_dirs_are_0750_and_files_are_restricted() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("dirs");
        let nested = root.join("a").join("b");
        create_dir_all(&nested).unwrap();
        // umask 只會再移除位元；測試環境的 umask 不會移除 0750 中的位元（022 或 002）
        assert_eq!(mode_of(&root), DIR_MODE);
        assert_eq!(mode_of(&nested), DIR_MODE);

        // 新檔案：0640
        let path = nested.join("f");
        let file = private_std_options(std::fs::OpenOptions::new().create(true).append(true))
            .open(&path)
            .unwrap();
        assert_eq!(mode_of(&path), FILE_MODE);
        assert!(!restrict_open_file(&file).unwrap());

        // 既有的寬鬆檔案：收緊；0600 不放寬
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(restrict_open_file(&file).unwrap());
        assert_eq!(mode_of(&path), 0o640);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o604)).unwrap();
        restrict_path(&path).unwrap();
        assert_eq!(mode_of(&path), 0o600);

        // 目錄權限警告
        assert_eq!(dir_permission_warning(&root, "資料目錄"), None);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let w = dir_permission_warning(&root, "資料目錄").unwrap();
        assert!(w.contains("0755") && w.contains("chmod 750"), "{w}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn async_variants_match() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("async");
        create_dir_all_async(&root.join("x")).await.unwrap();
        assert_eq!(mode_of(&root), DIR_MODE);
        let path = root.join("x").join("f");
        let file = private_tokio_options(tokio::fs::OpenOptions::new().create(true).append(true))
            .open(&path)
            .await
            .unwrap();
        assert_eq!(mode_of(&path), FILE_MODE);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(restrict_open_tokio_file(&file).await.unwrap());
        assert_eq!(mode_of(&path), 0o640);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn key_permission_warning_checks_mode_and_group() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("key");
        create_dir_all(&root).unwrap();
        let key = root.join("key.pem");
        std::fs::write(&key, b"x").unwrap();
        let set =
            |mode| std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).unwrap();

        set(0o600);
        assert_eq!(key_permission_warning(&key), None);
        // 測試建立的檔案屬於行程自己的群組：640 不警告
        set(0o640);
        assert_eq!(key_permission_warning(&key), None);
        set(0o644);
        let w = key_permission_warning(&key).unwrap();
        assert!(w.contains("0644") && w.contains("chmod 640"), "{w}");
        set(0o660);
        assert!(key_permission_warning(&key).is_some());

        // Linux 上以 root 執行時（例如 Docker 中的測試）：群組換成別的 GID，640 也要警告
        #[cfg(target_os = "linux")]
        if std::os::unix::fs::chown(&key, None, Some(54321)).is_ok() {
            set(0o640);
            let w = key_permission_warning(&key).unwrap();
            assert!(w.contains("GID 54321"), "{w}");
        }
        // 讀不到時不警告（建立 TLS 設定時另外報錯）
        assert_eq!(key_permission_warning(&root.join("missing.pem")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_only_creates_dirs() {
        let root = std::env::temp_dir().join(format!(
            "kairos-fsperm-win-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        create_dir_all(&root.join("a")).unwrap();
        assert!(root.join("a").is_dir());
        assert_eq!(dir_permission_warning(&root, "資料目錄"), None);
        assert_eq!(key_permission_warning(&root.join("a")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
