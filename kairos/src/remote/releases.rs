//! Pulse 版本庫（REMOTE.md 第 4.1 節）：匯入一次發佈的 `SHA256SUMS`、`SHA256SUMS.sig` 與發佈清單，
//! 以 `[remote] allowed_signers` 驗證（`release::verify_bundle`；`KRS-E8413`），之後逐一上傳發佈檔
//! （必須列在 `SHA256SUMS` 中且雜湊相符），以 SHA-256 命名存入 `<releases_dir>/<sha256>`（目錄 0700）。
//!
//! Kairos 的驗證只是**提早發現錯誤**，信任仍由每台 Pulse 以本機的信任錨決定（REMOTE.md 第 4.2 節）；
//! 沒有設定 `allowed_signers` 時只檢查格式與雜湊（`signer_fingerprint` 為 `None`，主控台警告）。

use ring::digest;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use kairos_proto::mgmt::{MAX_MANIFEST_BYTES, MAX_SHA256SUMS_BYTES, MAX_SIGNATURE_BYTES};
use kairos_proto::release::{
    self, AllowedSigners, Format, Product, ReleaseBundle, ReleaseError, ReleaseFile,
    ReleaseManifest, Sha256Sums, SshSignature, TrustPolicy, Version,
};

use super::store::ArtifactRow;

/// 匯入失敗（`KRS-E8413`）的分類：API 的錯誤碼
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// 簽章驗證失敗（400 `release_signature_invalid`）
    Signature(String),
    /// 格式、清單、雜湊不符或不是 Pulse 的發佈（400 `release_invalid`）
    Invalid(String),
}

/// 一次發佈的簽章資料（API 的本文、CLI 讀取的目錄）
#[derive(Debug, Clone, Copy)]
pub struct ImportRequest<'a> {
    pub sha256sums: &'a str,
    pub signature: &'a str,
    /// 發佈清單的內容（0.9.0 以前的發佈沒有）
    pub manifest: Option<&'a str>,
}

/// 驗證通過的發佈
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRelease {
    pub version: String,
    pub commit: String,
    /// 以 `[remote] allowed_signers` 驗證時的 (principal, 指紋)
    pub signer: Option<(String, String)>,
    /// 簽章金鑰的指紋（沒有驗證時也有：讓管理員比對）
    pub key_fingerprint: String,
    pub manifest_name: Option<String>,
    pub manifest: Option<ReleaseManifest>,
    /// `SHA256SUMS` 中這個版本的 Pulse 發佈檔（`release_id` 為 0、`stored = false`）
    pub artifacts: Vec<ArtifactRow>,
}

/// `allowed_signers` 檔案的大小上限（bytes）
pub const MAX_ALLOWED_SIGNERS_BYTES: u64 = 64 * 1024;

/// 信任設定：`[remote] allowed_signers` 的內容與 `extra_principals`
#[derive(Debug, Clone)]
pub struct Trust {
    pub signers: AllowedSigners,
    pub extra_principals: Vec<String>,
}

/// 讀取並解析 `allowed_signers`（大小上限 64 KiB）。不能用的行另記 WARN；沒有任何可用的行時錯誤
pub fn load_trust(path: &Path, extra_principals: Vec<String>) -> Result<Trust, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}：{e}", path.display()))?;
    if meta.len() > MAX_ALLOWED_SIGNERS_BYTES {
        return Err(format!("{} 超過 64 KiB", path.display()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}：{e}", path.display()))?;
    let signers = AllowedSigners::parse(&text);
    for r in &signers.rejected {
        log::warn!(
            "remote.allowed_signers {} 第 {} 行不使用：{}",
            path.display(),
            r.line,
            r.reason
        );
    }
    if !signers.has_usable_entry(release::RELEASE_NAMESPACE) {
        return Err(format!(
            "{} 沒有可用的 ssh-ed25519 金鑰（需要 namespaces=\"{}\"）",
            path.display(),
            release::RELEASE_NAMESPACE
        ));
    }
    Ok(Trust {
        signers,
        extra_principals,
    })
}

fn integrity(e: impl std::fmt::Display) -> ImportError {
    ImportError::Invalid(e.to_string())
}

/// 驗證一次發佈（REMOTE.md 第 4.2.2 節步驟 1～3）並列出 `SHA256SUMS` 中的 Pulse 發佈檔
pub fn verify_release(
    req: &ImportRequest<'_>,
    trust: Option<&Trust>,
    now_unix: i64,
) -> Result<VerifiedRelease, ImportError> {
    if req.sha256sums.is_empty() || req.sha256sums.len() > MAX_SHA256SUMS_BYTES {
        return Err(integrity(format!(
            "SHA256SUMS 必須是 1～{MAX_SHA256SUMS_BYTES} bytes"
        )));
    }
    if req.signature.is_empty() || req.signature.len() > MAX_SIGNATURE_BYTES {
        return Err(integrity(format!(
            "SHA256SUMS.sig 必須是 1～{MAX_SIGNATURE_BYTES} bytes"
        )));
    }
    if req.manifest.is_some_and(|m| m.len() > MAX_MANIFEST_BYTES) {
        return Err(integrity(format!(
            "發佈清單最多 {MAX_MANIFEST_BYTES} bytes"
        )));
    }
    let sig = SshSignature::parse(req.signature)
        .map_err(|e| ImportError::Signature(format!("SHA256SUMS.sig 無法解析：{e}")))?;
    let sums = Sha256Sums::parse(req.sha256sums).map_err(integrity)?;
    // 清單的檔名由內容決定（`pulse-<版本>-<commit>-release.json`），必須列在 SHA256SUMS 中
    let manifest_name = match req.manifest {
        Some(content) => {
            let m = ReleaseManifest::parse(content).map_err(integrity)?;
            Some(m.file_name())
        }
        None => None,
    };
    let bundle = ReleaseBundle {
        sha256sums: req.sha256sums,
        signature: req.signature,
        manifest: manifest_name.as_deref().zip(req.manifest),
    };
    let (signer, manifest) = match trust {
        Some(t) => {
            let v = release::verify_bundle(
                &bundle,
                &TrustPolicy {
                    signers: &t.signers,
                    extra_principals: &t.extra_principals,
                    now_unix,
                },
            )
            .map_err(|e| match e {
                ReleaseError::Signature(s) => ImportError::Signature(format!(
                    "簽章驗證失敗：{s}（金鑰 {}）",
                    sig.fingerprint()
                )),
                ReleaseError::Integrity(s) => ImportError::Invalid(s),
            })?;
            (Some((v.signer.principal, v.signer.fingerprint)), v.manifest)
        }
        None => {
            // 沒有信任錨：只確認簽章本身（namespace、雜湊演算法、Ed25519）與清單的雜湊
            sig.verify_message(release::RELEASE_NAMESPACE, req.sha256sums.as_bytes())
                .map_err(|e| ImportError::Signature(format!("簽章無效：{e}")))?;
            let manifest = match (manifest_name.as_deref(), req.manifest) {
                (Some(name), Some(content)) => {
                    sums.check(name, &release::sha256_hex(content.as_bytes()))
                        .map_err(integrity)?;
                    Some(ReleaseManifest::parse(content).map_err(integrity)?)
                }
                _ => None,
            };
            (None, manifest)
        }
    };
    if let Some(m) = &manifest
        && m.product() != Product::Pulse
    {
        return Err(integrity(
            "版本庫只接受 Pulse 的發佈（Kairos 自身依 INSTALL.md 手動升級）",
        ));
    }
    let mut version: Option<(Version, String)> =
        manifest.as_ref().map(|m| (m.version(), m.commit.clone()));
    let mut artifacts = Vec::new();
    for (name, hash) in sums.entries() {
        if Some(name) == manifest_name.as_deref() {
            continue;
        }
        let Ok(file) = ReleaseFile::parse(name) else {
            continue; // 安裝指令碼、SBOM 以外的其他檔案
        };
        if file.product != Product::Pulse || file.format == Format::CdxJson {
            continue;
        }
        if file.dirty {
            return Err(integrity(format!("{name} 是有未提交變更的建置，不能匯入")));
        }
        match &version {
            Some((v, c)) if *v != file.version || *c != file.commit => {
                return Err(integrity(format!(
                    "{name} 的版本或 commit 與這次發佈（{v} {c}）不一致"
                )));
            }
            Some(_) => {}
            None => version = Some((file.version.clone(), file.commit.clone())),
        }
        artifacts.push(ArtifactRow {
            release_id: 0,
            filename: name.to_string(),
            os: file.os.as_str().to_string(),
            arch: file.arch.as_str().to_string(),
            format: file.format.extension().to_string(),
            sha256: hash.to_string(),
            size: 0,
            stored: false,
            stored_at: None,
        });
    }
    let Some((version, commit)) = version else {
        return Err(integrity("SHA256SUMS 中沒有 Pulse 的發佈檔"));
    };
    if artifacts.is_empty() {
        return Err(integrity("SHA256SUMS 中沒有 Pulse 的發佈檔"));
    }
    if version.build.is_some() {
        return Err(integrity("版本不得帶 build metadata（+…）"));
    }
    Ok(VerifiedRelease {
        version: version.to_string(),
        commit,
        signer,
        key_fingerprint: sig.fingerprint(),
        manifest_name,
        manifest,
        artifacts,
    })
}

/// 建立版本庫目錄（Unix 0700）
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// 版本庫中的檔案路徑
pub fn artifact_path(dir: &Path, sha256: &str) -> PathBuf {
    dir.join(sha256)
}

/// 版本庫中已存入（`stored`）的發佈檔是否還在（只 `stat`，不重算雜湊）：不存在或不是一般檔案為
/// `Some("missing")`，大小與記錄不符為 `Some("size_mismatch")`，正常為 `None`。備份不含發佈檔
/// （REMOTE.md 第 6 節），還原後由系統健康與 `kairos releases list` 標出
pub fn file_problem(dir: &Path, sha256: &str, size: i64) -> Option<&'static str> {
    match std::fs::metadata(artifact_path(dir, sha256)) {
        Ok(m) if m.is_file() => (size > 0 && m.len() != size as u64).then_some("size_mismatch"),
        _ => Some("missing"),
    }
}

/// 寫入中的上傳：暫存檔、SHA-256 與大小
pub struct Upload {
    dir: PathBuf,
    tmp: PathBuf,
    file: Option<std::fs::File>,
    ctx: digest::Context,
    size: u64,
    max: u64,
}

impl Upload {
    pub fn create(dir: &Path, max: u64) -> std::io::Result<Upload> {
        ensure_dir(dir)?;
        let mut rnd = [0u8; 8];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut rnd)
            .map_err(|_| std::io::Error::other("無法產生隨機數"))?;
        let tmp = dir.join(format!(
            ".upload-{}",
            rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        crate::fsperm::private_std_options(&mut opts);
        let file = opts.open(&tmp)?;
        Ok(Upload {
            dir: dir.to_path_buf(),
            tmp,
            file: Some(file),
            ctx: digest::Context::new(&digest::SHA256),
            size: 0,
            max,
        })
    }

    /// 寫入一段；超過上限時錯誤（`Err(None)`），I/O 失敗時 `Err(Some(e))`
    pub fn write(&mut self, data: &[u8]) -> Result<(), Option<std::io::Error>> {
        self.size += data.len() as u64;
        if self.size > self.max {
            return Err(None);
        }
        self.ctx.update(data);
        match self.file.as_mut() {
            Some(f) => f.write_all(data).map_err(Some),
            None => Err(Some(std::io::Error::other("上傳已結束"))),
        }
    }

    /// 寫完：fsync 後回傳 (SHA-256, 大小)；檔案仍是暫存檔
    pub fn finish(&mut self) -> std::io::Result<(String, u64)> {
        if let Some(f) = self.file.take() {
            f.sync_all()?;
        }
        let d = self.ctx.clone().finish();
        Ok((
            d.as_ref().iter().map(|b| format!("{b:02x}")).collect(),
            self.size,
        ))
    }

    /// 把暫存檔改名為 `<sha256>`（已存在相同內容的檔案時直接刪除暫存檔）
    pub fn commit(mut self, sha256: &str) -> std::io::Result<PathBuf> {
        let dest = artifact_path(&self.dir, sha256);
        // 已有同名的檔案而大小不同（被截斷或換掉，系統健康的「版本庫的檔案遺失」）：以這次上傳的取代
        let same = std::fs::metadata(&dest).is_ok_and(|m| m.is_file() && m.len() == self.size);
        if same {
            let _ = std::fs::remove_file(&self.tmp);
        } else {
            std::fs::rename(&self.tmp, &dest)?;
        }
        // 已改名：Drop 不再刪除
        self.tmp = PathBuf::new();
        Ok(dest)
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if !self.tmp.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// 讀取發佈檔的一段（`upgrade_chunk`）：回傳 (資料, 是否到檔尾)
pub fn read_chunk(
    path: &Path,
    offset: u64,
    length: u32,
    size: u64,
) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::{Seek, SeekFrom};
    if offset >= size {
        return Ok((Vec::new(), true));
    }
    let want = u64::from(length).min(size - offset) as usize;
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; want];
    let mut got = 0;
    while got < want {
        let n = f.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    if got < want {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "發佈檔比記錄的大小短（檔案可能被修改）",
        ));
    }
    buf.truncate(got);
    Ok((buf, offset + got as u64 >= size))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/kairos-proto/testdata/release/"
    );

    fn read(name: &str) -> String {
        std::fs::read_to_string(format!("{DIR}{name}")).unwrap()
    }

    fn trust() -> Trust {
        Trust {
            signers: AllowedSigners::parse(&read("allowed_signers")),
            extra_principals: Vec::new(),
        }
    }

    const NOW: i64 = 1_790_000_000;

    #[test]
    fn the_test_vector_release_verifies_and_lists_pulse_artifacts() {
        let sums = read("SHA256SUMS");
        let sig = read("SHA256SUMS.sig");
        let manifest = read("pulse-0.9.1-5e6f7a8-release.json");
        let req = ImportRequest {
            sha256sums: &sums,
            signature: &sig,
            manifest: Some(&manifest),
        };
        let v = verify_release(&req, Some(&trust()), NOW).unwrap();
        assert_eq!(
            (v.version.as_str(), v.commit.as_str()),
            ("0.9.1", "5e6f7a8")
        );
        assert_eq!(v.signer.as_ref().unwrap().0, "release@kairos-pulse");
        assert_eq!(v.signer.as_ref().unwrap().1, v.key_fingerprint);
        assert_eq!(
            v.manifest_name.as_deref(),
            Some("pulse-0.9.1-5e6f7a8-release.json")
        );
        let names: Vec<&str> = v.artifacts.iter().map(|a| a.filename.as_str()).collect();
        assert_eq!(
            names,
            [
                "pulse-0.9.1-5e6f7a8-linux-x86_64.deb",
                "pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz"
            ]
        );
        assert_eq!(v.artifacts[1].format, "tar.gz");
        // 沒有信任錨：只檢查簽章本身與雜湊
        let v2 = verify_release(&req, None, NOW).unwrap();
        assert!(v2.signer.is_none());
        assert_eq!(v2.key_fingerprint, v.key_fingerprint);
    }

    #[test]
    fn tampered_or_foreign_releases_are_rejected() {
        let sums = read("SHA256SUMS");
        let manifest = read("pulse-0.9.1-5e6f7a8-release.json");
        let other = read("SHA256SUMS.other-key.sig");
        let req = ImportRequest {
            sha256sums: &sums,
            signature: &other,
            manifest: Some(&manifest),
        };
        assert!(matches!(
            verify_release(&req, Some(&trust()), NOW),
            Err(ImportError::Signature(_))
        ));
        let wrong_ns = read("SHA256SUMS.wrong-namespace.sig");
        let req = ImportRequest {
            signature: &wrong_ns,
            ..req
        };
        assert!(matches!(
            verify_release(&req, None, NOW),
            Err(ImportError::Signature(_))
        ));
        let sig = read("SHA256SUMS.sig");
        let tampered = sums.replace("a13150fd", "b13150fd");
        let req = ImportRequest {
            sha256sums: &tampered,
            signature: &sig,
            manifest: Some(&manifest),
        };
        assert!(matches!(
            verify_release(&req, Some(&trust()), NOW),
            Err(ImportError::Signature(_))
        ));
        // 清單內容被改：雜湊不符
        let m2 = manifest.replace("\"breaking\":false", "\"breaking\":true");
        let req = ImportRequest {
            sha256sums: &sums,
            signature: &sig,
            manifest: Some(&m2),
        };
        assert!(matches!(
            verify_release(&req, Some(&trust()), NOW),
            Err(ImportError::Invalid(_))
        ));
    }

    #[test]
    fn uploads_are_hashed_and_renamed_by_content() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-releases-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        let mut up = Upload::create(&dir, 10).unwrap();
        up.write(b"hello").unwrap();
        let (sha, size) = up.finish().unwrap();
        assert_eq!(size, 5);
        assert_eq!(sha, release::sha256_hex(b"hello"));
        let path = up.commit(&sha).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        let (data, eof) = read_chunk(&path, 1, 3, 5).unwrap();
        assert_eq!((data.as_slice(), eof), (&b"ell"[..], false));
        let (data, eof) = read_chunk(&path, 3, 32768, 5).unwrap();
        assert_eq!((data.as_slice(), eof), (&b"lo"[..], true));
        assert_eq!(read_chunk(&path, 5, 10, 5).unwrap(), (Vec::new(), true));
        assert!(
            read_chunk(&path, 0, 10, 9).is_err(),
            "file shorter than recorded"
        );
        let mut big = Upload::create(&dir, 4).unwrap();
        assert!(matches!(big.write(b"hello"), Err(None)));
        drop(big);
        // 只剩下 commit 的檔案（暫存檔已刪除）
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(left.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
