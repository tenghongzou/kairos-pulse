//! Pulse 與特權輔助程式之間的檔案（`crates/kairos-proto/REMOTE.md` 第 4.4.1、4.6 節）：
//!
//! - `<狀態目錄>/upgrade/<upgrade_id>/`：Pulse（服務帳號）暫存的發佈檔、`SHA256SUMS`、`.sig`、發佈清單、
//!   deb／rpm 的回滾檔（與它的 `SHA256SUMS`、`.sig`，只在本機有驗證過的紀錄時）。
//! - `<狀態目錄>/upgrade/request.json`（[`Request`]）：最後以寫暫存檔再改名的方式寫入，觸發
//!   `pulse-upgrade.path`。輔助程式完成（成功、失敗或回滾）後刪除。
//! - `<狀態目錄>/upgrade/result.json`（[`UpgradeResult`]）：輔助程式寫，替換後的 Pulse 讀取後回報。
//! - `<狀態目錄>/upgrade/state.json`：Pulse 自己的狀態機（[`super::StateFile`]）。
//!
//! 輔助程式（root）不信任這些檔案的內容：只接受一般檔案（`O_NOFOLLOW`、`st_nlink == 1`、大小上限），
//! 複製到 root 專用的目錄後重新驗證。

use std::io::Read;
use std::path::{Path, PathBuf};

use kairos_proto::mgmt::{MAX_ARTIFACT_BYTES, UpgradeTarget};
use serde::{Deserialize, Serialize};

/// `request.json` 的 `schema`
pub const REQUEST_SCHEMA: &str = "pulse.upgrade-request/v1";
/// `result.json` 的 `schema`
pub const RESULT_SCHEMA: &str = "pulse.upgrade-result/v1";
/// `request.json` 的上限（REMOTE.md 第 4.4.1 節）
pub const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// `result.json` 的上限
pub const MAX_RESULT_BYTES: u64 = 64 * 1024;
/// 狀態目錄下的升級目錄
pub const UPGRADE_DIR: &str = "upgrade";
pub const REQUEST_FILE: &str = "request.json";
pub const RESULT_FILE: &str = "result.json";
pub const STATE_FILE: &str = "state.json";
/// 暫存目錄中簽章資料的檔名
pub const SUMS_FILE: &str = "SHA256SUMS";
pub const SIG_FILE: &str = "SHA256SUMS.sig";
/// 回滾檔所屬發佈的簽章資料（本機驗證過的紀錄，[`super::releases_dir`]）
pub const ROLLBACK_SUMS_FILE: &str = "rollback.SHA256SUMS";
pub const ROLLBACK_SIG_FILE: &str = "rollback.SHA256SUMS.sig";

/// 交給輔助程式的請求（`request.json`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub schema: String,
    pub upgrade_id: String,
    pub target: UpgradeTarget,
    /// `tar`、`deb`、`rpm`
    pub method: String,
    /// 發佈檔名（暫存目錄中）
    pub artifact: String,
    /// 發佈清單的檔名（暫存目錄中；0.9.0 以前的發佈沒有）
    pub manifest: Option<String>,
    /// deb／rpm 的回滾檔名（暫存目錄中；沒有時 `None`）
    pub rollback_artifact: Option<String>,
    /// 實際使用的健康確認期限（秒，`effective_health_timeout_secs`）
    pub health_timeout_secs: u32,
    /// 推送是否允許降版（本機的 `upgrade.allow_downgrade` 由輔助程式自己從 `pulse.toml` 讀）
    pub allow_downgrade: bool,
    /// 替換前的版本
    pub from_version: String,
    /// 寫入的時間（RFC 3339）
    pub requested_at: String,
}

/// 輔助程式的結果（`result.json`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeResult {
    pub schema: String,
    pub upgrade_id: String,
    /// `succeeded`、`failed`、`rolled_back`、`rollback_failed`
    pub state: String,
    pub code: Option<String>,
    #[serde(default)]
    pub detail: String,
    pub from_version: String,
    pub to_version: String,
    pub finished_at: String,
}

impl Request {
    /// 解析並檢查已讀入的 `request.json` 內容（輔助程式相對請求目錄的 fd 讀入後呼叫，見
    /// [`super::helper`] 的 `ReqDir`；讀入時已做 `O_NOFOLLOW`、一般檔案、`st_nlink == 1`、≤ 64 KiB）
    pub fn from_untrusted_bytes(bytes: &[u8]) -> Result<Self, String> {
        let r: Request =
            serde_json::from_slice(bytes).map_err(|e| format!("不是合法的 JSON：{e}"))?;
        r.validate()?;
        Ok(r)
    }

    /// 欄位的限制（檔名只能是單純的檔名，且與目標版本一致）
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != REQUEST_SCHEMA {
            return Err(format!("request.json 的 schema 不是 {REQUEST_SCHEMA}"));
        }
        if !kairos_proto::is_uuid_text(&self.upgrade_id) {
            return Err("request.json 的 upgrade_id 不是 UUID".to_string());
        }
        if !matches!(self.method.as_str(), "tar" | "deb" | "rpm") {
            return Err(format!("不支援的安裝方式 {:?}", self.method));
        }
        for name in std::iter::once(&self.artifact)
            .chain(self.manifest.as_ref())
            .chain(self.rollback_artifact.as_ref())
        {
            if !is_plain_name(name) {
                return Err(format!("request.json 中的檔名 {name:?} 不是單純的檔名"));
            }
        }
        kairos_proto::release::ReleaseFile::parse(&self.artifact)?;
        if let Some(m) = &self.manifest {
            kairos_proto::release::ReleaseFile::parse_manifest_name(m)?;
        }
        if let Some(r) = &self.rollback_artifact {
            kairos_proto::release::ReleaseFile::parse(r)?;
        }
        // 只檢查格式：`from_version` 是服務帳號寫的，輔助程式另以自己的版本（或狀態檔記下的版本）比對，
        // 不符就拒絕（`helper.rs` 的 `begin`）
        if kairos_proto::release::Version::parse(&self.from_version).is_err() {
            return Err(format!(
                "request.json 的 from_version {:?} 不是合法的版本",
                self.from_version
            ));
        }
        // `target` 也是服務帳號寫的自由字串，且會原樣寫進 `result.json` 的 `to_version`、journal 與
        // `upgrade.log`。限制成合法的版本與 commit，避免把 `$( ) ` 換行等字元帶進會被 shell source 的
        // 檔案（安全稽核 2026-11-14）。輔助程式之後另以發佈檔名、簽章、清單重新驗證實際的版本與 commit
        if kairos_proto::release::Version::parse(&self.target.version).is_err() {
            return Err(format!(
                "request.json 的 target.version {:?} 不是合法的版本",
                self.target.version
            ));
        }
        if !kairos_proto::release::is_commit(&self.target.commit) {
            return Err(format!(
                "request.json 的 target.commit {:?} 不是 7..=40 個小寫十六進位",
                self.target.commit
            ));
        }
        Ok(())
    }
}

impl UpgradeResult {
    /// 讀取 `result.json`（Pulse；不存在時 `Ok(None)`）
    pub fn read(path: &Path) -> Result<Option<Self>, String> {
        match crate::remote::read_small(path, MAX_RESULT_BYTES) {
            Ok(bytes) => {
                let r: UpgradeResult = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("{} 無法解析：{e}", path.display()))?;
                if r.schema != RESULT_SCHEMA {
                    return Err(format!("{} 的 schema 不是 {RESULT_SCHEMA}", path.display()));
                }
                Ok(Some(r))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("無法讀取 {}：{e}", path.display())),
        }
    }
}

/// 單純的檔名（不含路徑分隔、不是 `.`／`..`、不含控制字元）
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control)
}

/// 以不跟隨符號連結的方式開啟一般檔案，檢查硬連結數與大小後讀入（輔助程式讀 Pulse 寫的檔案）
pub fn read_untrusted_file(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let mut f = open_untrusted(path, max)?;
    let mut buf = Vec::new();
    f.by_ref()
        .take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("無法讀取 {}：{e}", path.display()))?;
    if buf.len() as u64 > max {
        return Err(format!("{} 超過 {max} bytes", path.display()));
    }
    Ok(buf)
}

/// 開啟 Pulse 寫的檔案：`O_NOFOLLOW`、一般檔案、`st_nlink == 1`、不超過 `max` bytes
pub fn open_untrusted(path: &Path, max: u64) -> Result<std::fs::File, String> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = o
        .open(path)
        .map_err(|e| format!("無法開啟 {}（不接受符號連結）：{e}", path.display()))?;
    check_untrusted_meta(&f, &path.display().to_string(), max)?;
    Ok(f)
}

/// 檢查已開啟的 Pulse 檔案：一般檔案、`st_nlink == 1`、不超過 `max` bytes。路徑開啟（[`open_untrusted`]）
/// 與相對目錄 fd 的 `openat`（輔助程式的 `ReqDir`）共用
pub fn check_untrusted_meta(f: &std::fs::File, display: &str, max: u64) -> Result<(), String> {
    let meta = f
        .metadata()
        .map_err(|e| format!("無法讀取 {display} 的屬性：{e}"))?;
    if !meta.file_type().is_file() {
        return Err(format!("{display} 不是一般檔案"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Err(format!("{display} 有多個硬連結，拒絕使用"));
        }
    }
    if meta.len() > max {
        return Err(format!(
            "{display} 超過大小上限（{} > {max} bytes）",
            meta.len()
        ));
    }
    Ok(())
}

/// 把已開啟、已通過 [`check_untrusted_meta`] 的來源串流到 `dst`（新建、0600），回傳 SHA-256。
/// 路徑來源（`open_untrusted` + 這個函式）與相對 fd 的來源（輔助程式的 `ReqDir`）共用
pub fn copy_checked(
    mut f: std::fs::File,
    src_display: &str,
    dst: &Path,
    max: u64,
) -> Result<String, String> {
    let mut out = crate::remote::open_new_file(dst, 0o600)
        .map_err(|e| format!("無法建立 {}：{e}", dst.display()))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("無法讀取 {src_display}：{e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max {
            return Err(format!("{src_display} 超過 {max} bytes"));
        }
        ctx.update(&buf[..n]);
        std::io::Write::write_all(&mut out, &buf[..n])
            .map_err(|e| format!("無法寫入 {}：{e}", dst.display()))?;
    }
    out.sync_all()
        .map_err(|e| format!("無法寫入 {}：{e}", dst.display()))?;
    Ok(hex(ctx.finish().as_ref()))
}

/// 檔案的 SHA-256（小寫十六進位）
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}

/// 小寫十六進位
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 發佈檔的大小上限（與 `upgrade_offer` 相同）
pub const MAX_FILE_BYTES: u64 = MAX_ARTIFACT_BYTES;

/// 升級目錄下的路徑
pub fn upgrade_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(UPGRADE_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_only() {
        for ok in ["pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz", "SHA256SUMS"] {
            assert!(is_plain_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a\\b", "x\ny", &"a".repeat(256)] {
            assert!(!is_plain_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn request_validation_rejects_paths_and_unknown_methods() {
        let ok = Request {
            schema: REQUEST_SCHEMA.into(),
            upgrade_id: "9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70".into(),
            target: UpgradeTarget {
                version: "0.9.1".into(),
                commit: "5e6f7a8".into(),
            },
            method: "tar".into(),
            artifact: "pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz".into(),
            manifest: Some("pulse-0.9.1-5e6f7a8-release.json".into()),
            rollback_artifact: None,
            health_timeout_secs: 600,
            allow_downgrade: false,
            from_version: "0.9.0".into(),
            requested_at: "2026-11-20T17:00:00.000Z".into(),
        };
        ok.validate().unwrap();
        let mut bad = ok.clone();
        bad.artifact = "../../etc/shadow".into();
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.method = "msi".into();
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.upgrade_id = "x".into();
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.schema = "other".into();
        assert!(bad.validate().is_err());
        // target.version／target.commit 是服務帳號寫的自由字串：含危險字元、換行或超長時拒絕
        // （安全稽核 2026-11-14；它們會原樣寫進 result.json 與日誌）
        for danger in [
            "0.9.1; $(touch /tmp/x)",
            "0.9.1`id`",
            "0.9.1\n[Unit]",
            "$(reboot)",
            &"9".repeat(300),
        ] {
            let mut bad = ok.clone();
            bad.target.version = danger.into();
            assert!(bad.validate().is_err(), "target.version {danger:?}");
        }
        for danger in ["", "zzzzzzz", "5e6", "5e6f7a8$(id)", &"a".repeat(41)] {
            let mut bad = ok.clone();
            bad.target.commit = danger.into();
            assert!(bad.validate().is_err(), "target.commit {danger:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_files_reject_links_and_oversize() {
        let dir = std::env::temp_dir().join(format!("pulse-untrusted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real");
        std::fs::write(&real, b"hello").unwrap();
        assert_eq!(read_untrusted_file(&real, 5).unwrap(), b"hello");
        assert!(read_untrusted_file(&real, 4).unwrap_err().contains("超過"));
        let sym = dir.join("sym");
        std::os::unix::fs::symlink(&real, &sym).unwrap();
        assert!(read_untrusted_file(&sym, 100).is_err());
        let hard = dir.join("hard");
        std::fs::hard_link(&real, &hard).unwrap();
        assert!(
            read_untrusted_file(&hard, 100)
                .unwrap_err()
                .contains("硬連結")
        );
        std::fs::remove_file(&hard).unwrap();
        let fifo = dir.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: 以合法的 C 字串路徑建立 FIFO
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(
            read_untrusted_file(&fifo, 100)
                .unwrap_err()
                .contains("一般檔案")
        );
        let dst = dir.join("copy");
        let sha = copy_checked(
            open_untrusted(&real, 100).unwrap(),
            &real.display().to_string(),
            &dst,
            100,
        )
        .unwrap();
        assert_eq!(sha, sha256_file(&real).unwrap());
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
