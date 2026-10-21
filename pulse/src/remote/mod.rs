//! 遠端管理（商業審計 F8，`crates/kairos-proto/REMOTE.md`）的 Pulse 端：
//!
//! - [`config`]：集中設定（P1）——合併（內建預設 < Kairos 下發 < `pulse.toml` < 環境變數）、驗證、
//!   快取（`<狀態目錄>/remote/config.json`）、確認期與崩潰保護、`config_status` 的內容。
//! - [`install`]：hello 的 `install`（安裝方式、遠端升級與集中設定的本機狀態，REMOTE.md 第 2.1、4.3 節）。
//! - [`upgrade`]：遠端升級（P2，Linux）——提議的評估、分段下載、驗證、暫存、交給特權輔助程式、
//!   重新啟動後接續回報；輔助程式 `pulse upgrade apply`（[`upgrade::helper`]）。
//! - [`conn`]：每條連線上 `config`／`upgrade` 訊息的收送（由 `network` 的連線迴圈呼叫）。
//!
//! 協定的型別與純函式一律來自 `kairos_proto::mgmt`、`kairos_proto::release`，這裡只有 Pulse 的行為。

pub mod cli;
pub mod config;
pub mod conn;
pub mod install;
pub mod upgrade;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 狀態目錄的權限（Unix）：遠端設定快取與升級暫存的上層目錄（REMOTE.md 第 3.7 節：目錄 0750、檔案 0640）
pub const DIR_MODE: u32 = 0o750;
/// 遠端設定快取、升級狀態檔的權限（Unix）
pub const FILE_MODE: u32 = 0o640;

/// 狀態目錄（REMOTE.md 第 3.7 節）：systemd 的 `STATE_DIRECTORY`（第一個絕對路徑，多實例為
/// `/var/lib/pulse/N`）；沒有時 Linux 等為 `/var/lib/pulse`，Windows 為 `%ProgramData%\Pulse`
/// （與本機狀態檔的預設位置相同目錄）
pub fn state_dir() -> PathBuf {
    crate::status_file::state_directory(std::env::var_os("STATE_DIRECTORY").as_deref())
        .unwrap_or_else(|| {
            crate::status_file::default_path()
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        })
}

/// 以「寫暫存檔、fsync、改名」原子地寫入 `path`（同一個目錄）。上層目錄不存在時建立（Unix 權限
/// `dir_mode`）；Unix 上檔案權限為 `mode`
pub fn write_atomic(path: &Path, content: &[u8], mode: u32, dir_mode: u32) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_dir_all_mode(dir, dir_mode)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "路徑沒有檔名"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    let result = (|| {
        // 暫存檔一律新建（O_EXCL、不跟隨符號連結）：特權輔助程式也用這個函式寫 Pulse 可寫的目錄
        // （result.json），不能讓服務帳號預先放好的符號連結把 root 的寫入導到別的檔案
        let _ = std::fs::remove_file(&tmp);
        let mut f = open_exclusive(&tmp, mode)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 建立目錄（含上層），Unix 上新建的目錄權限為 `mode`
pub fn create_dir_all_mode(dir: &Path, mode: u32) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    b.create(dir)?;
    // umask（pulse.service 為 UMask=0077）會讓新目錄更嚴格；依規格的權限設定最末層
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// 開新檔案（已存在時截斷），Unix 上權限為 `mode`
pub fn open_new_file(path: &Path, mode: u32) -> io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    Ok(f)
}

/// 新建檔案（已存在時失敗；Unix 上不跟隨符號連結），權限為 `mode`
fn open_exclusive(path: &Path, mode: u32) -> io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(mode).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    Ok(f)
}

/// 讀取不超過 `max` bytes 的小檔案（狀態檔、快取）；超過時錯誤（避免被換成大檔案時整個讀進記憶體）
pub fn read_small(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} 超過 {max} bytes", path.display()),
        ));
    }
    Ok(buf)
}

/// 遠端管理在這個行程中的共用狀態：集中設定與遠端升級（跨連線保留，連線迴圈經 `network::Settings` 取用）
#[derive(Debug)]
pub struct RemoteMgmt {
    pub config: Arc<config::CentralConfig>,
    pub upgrade: Arc<upgrade::UpgradeAgent>,
}

impl RemoteMgmt {
    pub fn new(
        config: Arc<config::CentralConfig>,
        upgrade: Arc<upgrade::UpgradeAgent>,
    ) -> Arc<Self> {
        Arc::new(Self { config, upgrade })
    }

    /// hello 的 `install`
    pub fn install_info(&self) -> kairos_proto::InstallInfo {
        let mut info = self.upgrade.install_info();
        info.config = Some(self.config.install_state().to_string());
        info
    }

    /// hello 另外宣告的遠端管理 feature（`kairos_proto::mgmt::client_mgmt_features`）
    pub fn features(&self) -> Vec<&'static str> {
        kairos_proto::mgmt::client_mgmt_features(self.config.declare(), &self.install_info())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("pulse-remote-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("x.json");
        write_atomic(&path, b"one", FILE_MODE, DIR_MODE).unwrap();
        write_atomic(&path, b"two", FILE_MODE, DIR_MODE).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, FILE_MODE);
            let mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, DIR_MODE);
        }
        assert!(read_small(&path, 2).is_err());
        assert_eq!(read_small(&path, 3).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_does_not_follow_a_planted_temp_symlink() {
        // 服務帳號預先在可寫的目錄放好暫存檔名的符號連結：寫入不可以經由它改到別的檔案
        let dir = std::env::temp_dir().join(format!("pulse-remote-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let tmp = dir.join(format!(".result.json.tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();
        write_atomic(&dir.join("result.json"), b"data", 0o644, 0o750).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_eq!(std::fs::read(dir.join("result.json")).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
