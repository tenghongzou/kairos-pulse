//! Kairos 的 PKI（`kairos/PKI.md`）：CSR 檢查、依範本簽發用戶端憑證、CRL、代理簽發 CA 的金鑰與憑證，
//! 以及 `data/pki/` 的檔案。
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`x509`] | 憑證與 CRL 的解析、PEM、序號 |
//! | [`csr`] | CSR 的檢查（ENROLL.md 第 15.1 節） |
//! | [`issue`] | 簽發器：用戶端憑證、CRL、新的簽發 CA 的金鑰與 CSR |
//! | [`store`] | `pki_cas`、`pki_certificates`、`pki_crls` 的讀寫 |
//! | [`ca`] | 管理者的 CA 工具：`pki init`（根 CA、伺服器憑證、簽發 CA）、`pki sign`、`pki gencrl`、撤銷清單的匯出 |
//!
//! 檔案（`<storage.data_dir>/pki/`，目錄 0700）：
//!
//! | 檔案 | 內容 | 權限 |
//! |---|---|---|
//! | `issuing-<ca_id>.key` | 代理簽發 CA 的私鑰（PKCS#8 PEM） | 0600 |
//! | `issuing-cas.pem` | 狀態為 `active`／`trusted` 的簽發 CA 憑證（mTLS 的信任錨之一；Kairos 寫出） | 0644 |
//! | `issuing-cas-retired.pem` | 狀態為 `retired` 的簽發 CA 憑證（經過它們的用戶端憑證鏈一律拒絕；Kairos 寫出） | 0644 |
//! | `crl/issuing.crl` | 各簽發 CA 目前的 CRL（PEM，Kairos 產生） | 0644 |
//! | `root-ca.pem` | `kairos pki init` 建立的根 CA 憑證（`pki.server_ca_path`；私鑰不在這裡） | 0644 |
//! | `server.pem`、`server.key` | `kairos pki init` 簽發的伺服器憑證與私鑰（8443／8444／9443） | 0644、0600 |

pub mod ca;
pub mod csr;
pub mod issue;
pub mod manage;
pub mod queue;
pub mod store;
pub mod x509;

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// `data_dir` 下的 PKI 目錄
pub const PKI_DIR: &str = "pki";
/// 簽發 CA 的信任錨（Kairos 由資料庫寫出）
pub const ISSUING_CAS_FILE: &str = "issuing-cas.pem";
/// 已退役的簽發 CA（Kairos 由資料庫寫出；mTLS 拒絕經過它們的鏈，PKI.md 第 15.4 節）
pub const RETIRED_CAS_FILE: &str = "issuing-cas-retired.pem";
/// Kairos 產生的 CRL（`pki/crl/` 下）
pub const ISSUING_CRL_FILE: &str = "issuing.crl";
/// CRL 目錄
pub const CRL_DIR: &str = "crl";

/// `<data_dir>/pki`
pub fn pki_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(PKI_DIR)
}

/// `<data_dir>/pki/issuing-cas.pem`
pub fn issuing_cas_path(data_dir: &Path) -> PathBuf {
    pki_dir(data_dir).join(ISSUING_CAS_FILE)
}

/// `<data_dir>/pki/issuing-cas-retired.pem`
pub fn retired_cas_path(data_dir: &Path) -> PathBuf {
    pki_dir(data_dir).join(RETIRED_CAS_FILE)
}

/// `<data_dir>/pki/crl/issuing.crl`
pub fn issuing_crl_path(data_dir: &Path) -> PathBuf {
    pki_dir(data_dir).join(CRL_DIR).join(ISSUING_CRL_FILE)
}

/// 簽發 CA 私鑰的檔名（相對於 `data_dir`，存入 `pki_cas.key_file`）
pub fn key_file_name(ca_id: i64) -> String {
    format!("{PKI_DIR}/issuing-{ca_id}.key")
}

/// 建立 `pki/`（0700）與 `pki/crl/`（0700）；既有目錄不變
pub fn ensure_pki_dir(data_dir: &Path) -> io::Result<PathBuf> {
    let dir = pki_dir(data_dir);
    for d in [dir.clone(), dir.join(CRL_DIR)] {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(&d)?;
    }
    Ok(dir)
}

/// 寫入私鑰：新檔、權限 0600（Unix）；檔案已存在時拒絕（不覆寫）
pub fn write_private_key(path: &Path, pem: &str) -> io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(pem.as_bytes())?;
    f.sync_all()
}

/// 原子寫入公開的檔案（暫存檔 → fsync → 更名；0644）
pub fn write_public_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o644);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// 簽發 CA 私鑰（與所在目錄）權限過寬的原因（`kairos/PKI.md` 第 3 節）：私鑰的群組或其他使用者有任何權限、
/// 擁有者不是執行 Kairos 的帳號，或目錄的群組或其他使用者有權限。沒問題或非 Unix 時為 `None`
pub fn key_permission_problem(key_path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = match std::fs::metadata(key_path) {
            Ok(m) => m,
            Err(e) => return Some(format!("無法讀取 {}：{e}", key_path.display())),
        };
        let mode = meta.permissions().mode() & 0o7777;
        if mode & 0o077 != 0 {
            return Some(format!(
                "私鑰權限過寬（{}：{mode:04o}，應為 0600）",
                key_path.display()
            ));
        }
        if let Some(uid) = crate::fsperm::effective_uid()
            && meta.uid() != uid
        {
            return Some(format!(
                "私鑰的擁有者（uid {}）不是執行 Kairos 的帳號（uid {uid}）",
                meta.uid()
            ));
        }
        if let Some(dir) = key_path.parent()
            && let Ok(d) = std::fs::metadata(dir)
        {
            let dmode = d.permissions().mode() & 0o7777;
            if dmode & 0o077 != 0 {
                return Some(format!(
                    "私鑰所在目錄的權限過寬（{}：{dmode:04o}，應為 0700）",
                    dir.display()
                ));
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = key_path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_key_is_written_once_with_owner_only_permission() {
        let dir = std::env::temp_dir().join(format!("kairos-pki-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pki = ensure_pki_dir(&dir).unwrap();
        let key = pki.join("issuing-1.key");
        write_private_key(&key, "secret").unwrap();
        assert!(write_private_key(&key, "again").is_err());
        assert_eq!(std::fs::read_to_string(&key).unwrap(), "secret");
        assert_eq!(key_permission_problem(&key), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
            assert!(key_permission_problem(&key).unwrap().contains("過寬"));
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::set_permissions(&pki, std::fs::Permissions::from_mode(0o750)).unwrap();
            assert!(key_permission_problem(&key).unwrap().contains("目錄"));
        }
        write_public_atomically(&pki.join("x.pem"), b"abc").unwrap();
        assert_eq!(std::fs::read(pki.join("x.pem")).unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
