//! 請求目錄（`/var/lib/pulse/upgrade`，由低權限的 `pulse` 服務帳號擁有、可寫）的防符號連結操作
//! （安全稽核 2026-11-14 A1；`crates/kairos-proto/REMOTE.md` 第 17.9.7 節）。
//!
//! 升級輔助程式以 root 執行，但請求目錄可由服務帳號改名、換成指向任意目錄的符號連結。[`ReqDir`] 在
//! 一開始就以 `O_DIRECTORY | O_NOFOLLOW` 開啟請求目錄取得一個 fd、`fstat` 確認是目錄且擁有者合理，之後
//! root 對這個目錄的所有操作——讀 `request.json`、寫 `result.json`、刪 `request.json`、刪暫存的
//! `<upgrade_id>` 子目錄、讀暫存的發佈檔——都相對這個 fd 以 `openat`／`renameat`／`unlinkat` 進行。
//! fd 握住的是開啟當下的那個 inode：服務帳號事後把路徑改名、換成符號連結，都改變不了 root 寫到哪裡、
//! 刪掉哪裡，關閉了「檢查到使用之間」（TOCTOU）的時間窗。上層目錄的符號連結同樣跟隨不了：開啟最後一段
//! 用 `O_NOFOLLOW`，若請求目錄本身已是符號連結則 `open` 以 `ELOOP` 失敗，輔助程式不做任何變更就結束。
//!
//! Windows（只用於交叉編譯檢查；輔助程式只在 Linux 執行）退回以路徑操作，維持原行為。

use std::path::Path;

#[cfg(unix)]
pub use imp::ReqDir;

#[cfg(unix)]
mod imp {
    use super::Path;
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    use crate::remote::upgrade::files;

    /// 以 fd 代表一個目錄；所有操作都相對這個 fd（`openat`／`renameat`／`unlinkat`），不重新解析路徑
    pub struct ReqDir {
        fd: OwnedFd,
        /// 只給錯誤訊息用（開啟當下的路徑）
        display: String,
    }

    fn cstr(name: &str) -> Result<CString, String> {
        CString::new(name).map_err(|_| format!("名稱 {name:?} 含有 NUL"))
    }

    /// 請求目錄可接受的擁有者：root、輔助程式自己執行的帳號（正式環境是 root）、或服務帳號（正式環境
    /// 的請求目錄由 `pulse` 建立、擁有）。這個檢查只排除「被換成某個無關帳號擁有的目錄」；取得服務帳號
    /// 的攻擊者本來就是請求目錄的合法擁有者，真正的保護是 `O_NOFOLLOW` 加上全程握住同一個 fd
    fn acceptable_owner(uid: u32) -> bool {
        // SAFETY: geteuid 沒有前置條件，也不會失敗
        let euid = unsafe { libc::geteuid() };
        uid == 0 || uid == euid || super::super::service_ids().map(|(u, _)| u) == Some(uid)
    }

    fn fstat(fd: RawFd) -> io::Result<libc::stat> {
        // SAFETY: fd 有效；傳入可寫的 stat 緩衝
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    impl ReqDir {
        /// 以 `O_DIRECTORY | O_NOFOLLOW` 開啟請求目錄，`fstat` 確認是目錄且擁有者合理
        pub fn open(dir: &Path) -> Result<Self, String> {
            let cpath = CString::new(dir.as_os_str().as_bytes())
                .map_err(|_| format!("{}: 路徑含有 NUL", dir.display()))?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: cpath 是有效的 C 字串；open 回傳新的 fd 或 -1
            let raw = unsafe { libc::open(cpath.as_ptr(), flags) };
            if raw < 0 {
                return Err(format!(
                    "{}: 無法以不跟隨符號連結的方式開啟請求目錄：{}",
                    dir.display(),
                    io::Error::last_os_error()
                ));
            }
            // SAFETY: raw 是剛取得、尚未被任何人持有的有效 fd
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let st =
                fstat(fd.as_raw_fd()).map_err(|e| format!("{}: 無法 fstat：{e}", dir.display()))?;
            if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
                return Err(format!("{}: 不是目錄", dir.display()));
            }
            if !acceptable_owner(st.st_uid) {
                return Err(format!(
                    "{}: 擁有者 uid {} 不是 root、服務帳號或輔助程式本身",
                    dir.display(),
                    st.st_uid
                ));
            }
            Ok(Self {
                fd,
                display: dir.display().to_string(),
            })
        }

        /// 讀入相對 fd 的一般檔案（`request.json`）
        pub fn read_file(&self, name: &str, max: u64) -> Result<Vec<u8>, String> {
            let (mut f, display) = self.open_regular(name, max)?;
            let mut buf = Vec::new();
            Read::by_ref(&mut f)
                .take(max + 1)
                .read_to_end(&mut buf)
                .map_err(|e| format!("無法讀取 {display}：{e}"))?;
            if buf.len() as u64 > max {
                return Err(format!("{display} 超過 {max} bytes"));
            }
            Ok(buf)
        }

        /// 相對 fd 開啟一般檔案（`openat`，不跟隨符號連結），套用與 [`files::open_untrusted`] 相同的檢查
        fn open_regular(&self, name: &str, max: u64) -> Result<(File, String), String> {
            let cname = cstr(name)?;
            let display = format!("{}/{name}", self.display);
            let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
            // SAFETY: fd 有效、cname 是有效的 C 字串
            let raw = unsafe { libc::openat(self.fd.as_raw_fd(), cname.as_ptr(), flags) };
            if raw < 0 {
                return Err(format!(
                    "無法開啟 {display}（不接受符號連結）：{}",
                    io::Error::last_os_error()
                ));
            }
            // SAFETY: raw 是剛取得的有效 fd
            let f = unsafe { File::from_raw_fd(raw) };
            files::check_untrusted_meta(&f, &display, max)?;
            Ok((f, display))
        }

        /// 把相對 fd 的暫存檔複製到 `dst`（0600），回傳 SHA-256
        pub fn copy_file(&self, name: &str, dst: &Path, max: u64) -> Result<String, String> {
            let (f, display) = self.open_regular(name, max)?;
            files::copy_checked(f, &display, dst, max)
        }

        /// 相對 fd 開啟一個子目錄（暫存的 `<upgrade_id>/`），不跟隨符號連結
        pub fn subdir(&self, name: &str) -> Result<ReqDir, String> {
            let cname = cstr(name)?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: fd 有效、cname 是有效的 C 字串
            let raw = unsafe { libc::openat(self.fd.as_raw_fd(), cname.as_ptr(), flags) };
            if raw < 0 {
                return Err(format!(
                    "無法開啟暫存目錄 {}/{name}（不接受符號連結）：{}",
                    self.display,
                    io::Error::last_os_error()
                ));
            }
            // SAFETY: raw 是剛取得的有效 fd
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            Ok(ReqDir {
                fd,
                display: format!("{}/{name}", self.display),
            })
        }

        /// 相對 fd 原子地寫入（`openat` 新建暫存檔 → fsync → `renameat` 取代 → fsync 目錄）。暫存檔用
        /// `O_EXCL | O_NOFOLLOW` 新建：服務帳號事先放好的同名符號連結不會讓 root 寫到別處
        pub fn write_atomic(&self, name: &str, content: &[u8], mode: u32) -> Result<(), String> {
            use std::os::unix::fs::PermissionsExt;
            let tmp = format!(".{name}.tmp-{}", std::process::id());
            let ctmp = cstr(&tmp)?;
            let cname = cstr(name)?;
            let dfd = self.fd.as_raw_fd();
            // SAFETY: dfd 有效、ctmp 是有效的 C 字串（殘留的暫存檔先清掉）
            unsafe { libc::unlinkat(dfd, ctmp.as_ptr(), 0) };
            let flags =
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: dfd 有效、ctmp 是有效的 C 字串；第四個可變參數是新檔案的 mode
            let raw = unsafe { libc::openat(dfd, ctmp.as_ptr(), flags, mode as libc::c_uint) };
            if raw < 0 {
                return Err(format!(
                    "無法建立 {}/{tmp}：{}",
                    self.display,
                    io::Error::last_os_error()
                ));
            }
            // SAFETY: raw 是剛取得的有效 fd
            let mut f = unsafe { File::from_raw_fd(raw) };
            let write = (|| -> io::Result<()> {
                // umask 可能讓新檔案更嚴格：依規格設回明確的權限
                f.set_permissions(std::fs::Permissions::from_mode(mode))?;
                f.write_all(content)?;
                f.sync_all()
            })();
            drop(f);
            if let Err(e) = write {
                // SAFETY: dfd/ctmp 有效
                unsafe { libc::unlinkat(dfd, ctmp.as_ptr(), 0) };
                return Err(format!("無法寫入 {}/{tmp}：{e}", self.display));
            }
            // SAFETY: dfd/ctmp/cname 皆有效
            if unsafe { libc::renameat(dfd, ctmp.as_ptr(), dfd, cname.as_ptr()) } < 0 {
                let e = io::Error::last_os_error();
                // SAFETY: dfd/ctmp 有效
                unsafe { libc::unlinkat(dfd, ctmp.as_ptr(), 0) };
                return Err(format!("無法改名為 {}/{name}：{e}", self.display));
            }
            // SAFETY: dfd 有效（改名要等目錄寫入磁碟才算持久）
            unsafe { libc::fsync(dfd) };
            Ok(())
        }

        /// 相對 fd 刪除一般檔案（`request.json`）；不存在視為成功
        pub fn remove_file(&self, name: &str) -> Result<(), String> {
            let cname = cstr(name)?;
            // SAFETY: fd 有效、cname 是有效的 C 字串
            if unsafe { libc::unlinkat(self.fd.as_raw_fd(), cname.as_ptr(), 0) } < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::ENOENT) {
                    return Err(format!("無法刪除 {}/{name}：{e}", self.display));
                }
            }
            Ok(())
        }

        /// 相對 fd 遞迴刪除 `name`（暫存的 `<upgrade_id>/`），全程不跟隨符號連結；不存在視為成功
        pub fn remove_dir_all(&self, name: &str) -> Result<(), String> {
            let cname = cstr(name)?;
            remove_entry(self.fd.as_raw_fd(), &cname)
                .map_err(|e| format!("無法刪除 {}/{name}：{e}", self.display))?;
            // SAFETY: fd 有效
            unsafe { libc::fsync(self.fd.as_raw_fd()) };
            Ok(())
        }
    }

    /// 遞迴刪除 `parent` 下名為 `name` 的項目（`fstatat`／`openat`／`unlinkat` 都不跟隨符號連結）
    fn remove_entry(parent: RawFd, name: &CStr) -> io::Result<()> {
        // SAFETY: parent/name 有效；AT_SYMLINK_NOFOLLOW 不跟隨最後一段
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(parent, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            return Err(e);
        }
        if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: parent/name 有效
            let raw = unsafe { libc::openat(parent, name.as_ptr(), flags) };
            if raw < 0 {
                let e = io::Error::last_os_error();
                // 剛才是目錄、現在卻無法以不跟隨連結開啟（已被換成符號連結）：刪掉那個連結本身
                if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) {
                    return unlink(parent, name, 0);
                }
                return Err(e);
            }
            remove_children(raw)?;
            unlink(parent, name, libc::AT_REMOVEDIR)
        } else {
            unlink(parent, name, 0)
        }
    }

    /// 遞迴刪除目錄 `fd` 底下的所有項目，然後關閉 `fd`（`fdopendir` 取得 `fd` 的所有權）
    fn remove_children(fd: RawFd) -> io::Result<()> {
        // SAFETY: fd 有效；fdopendir 接管 fd，closedir 會關閉它
        let dirp = unsafe { libc::fdopendir(fd) };
        if dirp.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: fdopendir 失敗時 fd 仍由我們持有
            unsafe { libc::close(fd) };
            return Err(e);
        }
        // 先收集所有名稱，再刪除：邊 readdir 邊 unlink 會跳過項目
        let mut names: Vec<CString> = Vec::new();
        loop {
            // SAFETY: dirp 有效
            let ent = unsafe { libc::readdir(dirp) };
            if ent.is_null() {
                break;
            }
            // SAFETY: ent 指向有效的 dirent；d_name 是 NUL 結尾
            let cname = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
            let b = cname.to_bytes();
            if b == b"." || b == b".." {
                continue;
            }
            names.push(cname.to_owned());
        }
        let mut result = Ok(());
        for n in &names {
            if let Err(e) = remove_entry(fd, n)
                && result.is_ok()
            {
                result = Err(e);
            }
        }
        // SAFETY: dirp 有效；closedir 關閉底層 fd
        unsafe { libc::closedir(dirp) };
        result
    }

    fn unlink(parent: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: parent/name 有效
        if unsafe { libc::unlinkat(parent, name.as_ptr(), flags) } < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ENOENT) {
                return Err(e);
            }
        }
        Ok(())
    }
}

#[cfg(not(unix))]
pub use fallback::ReqDir;

// Windows（交叉編譯檢查用）：沒有 `*at`，退回路徑操作，維持原行為
#[cfg(not(unix))]
mod fallback {
    use super::Path;
    use std::path::PathBuf;

    use crate::remote::upgrade::files;

    pub struct ReqDir {
        dir: PathBuf,
    }

    impl ReqDir {
        pub fn open(dir: &Path) -> Result<Self, String> {
            if !dir.is_dir() {
                return Err(format!("{}: 不是目錄", dir.display()));
            }
            Ok(Self {
                dir: dir.to_path_buf(),
            })
        }

        pub fn read_file(&self, name: &str, max: u64) -> Result<Vec<u8>, String> {
            files::read_untrusted_file(&self.dir.join(name), max)
        }

        pub fn copy_file(&self, name: &str, dst: &Path, max: u64) -> Result<String, String> {
            let src = self.dir.join(name);
            let f = files::open_untrusted(&src, max)?;
            files::copy_checked(f, &src.display().to_string(), dst, max)
        }

        pub fn subdir(&self, name: &str) -> Result<ReqDir, String> {
            ReqDir::open(&self.dir.join(name))
        }

        pub fn write_atomic(&self, name: &str, content: &[u8], mode: u32) -> Result<(), String> {
            crate::remote::write_atomic(&self.dir.join(name), content, mode, 0o750)
                .map_err(|e| e.to_string())
        }

        pub fn remove_file(&self, name: &str) -> Result<(), String> {
            match std::fs::remove_file(self.dir.join(name)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }

        pub fn remove_dir_all(&self, name: &str) -> Result<(), String> {
            match std::fs::remove_dir_all(self.dir.join(name)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}
