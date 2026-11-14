//! 發佈包的信任鏈（REMOTE.md 第 4.1.3、4.2、4.8 節；RELEASING.md 第 1、3 節）：Kairos 匯入版本庫時
//! （K2，`KRS-E8413`）、Pulse 收到提議時與特權輔助程式替換前（P2，`PLS-E8401`／`PLS-E8402`）共用。
//!
//! - [`sshsig`]：`SHA256SUMS.sig`（`ssh-keygen -Y sign -n kairos-pulse-release`）的驗證與
//!   `allowed_signers` 的解析，不呼叫系統的 `ssh-keygen`。
//! - [`Sha256Sums`]：`SHA256SUMS` 的解析與比對。
//! - [`ReleaseFile`]：發佈檔名 `<名稱>-<版本>-<commit>[-dirty]-<os>-<arch>.<格式>` 與
//!   清單檔名 `<名稱>-<版本>-<commit>-release.json`。
//! - [`ReleaseManifest`]：發佈清單（`min_kairos`、`min_upgrade_from`、`remote_features`、`unit`、`breaking`）。
//! - [`Version`]：SemVer 2.0 的解析與先後順序。
//! - [`verify_bundle`]／[`VerifiedBundle::check_artifact`]：REMOTE.md 第 4.2.2 節的步驟 1～4；
//!   [`check_version_policy`]：步驟 5 與第 4.8 節的降版政策。

pub mod sshsig;

pub use sshsig::{
    AllowedSigners, RejectedLine, SigError, SignerEntry, SshSignature, VerifiedSigner,
    key_fingerprint, verify_sshsig,
};

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

use crate::mgmt::is_sha256_hex;

/// 發佈簽章的 namespace（`ssh-keygen -Y sign -n`；RELEASING.md 第 3 節）
pub const RELEASE_NAMESPACE: &str = "kairos-pulse-release";
/// 官方發佈金鑰的 principal（`allowed_signers` 的第一欄）
pub const RELEASE_PRINCIPAL: &str = "release@kairos-pulse";
/// 發佈清單的 `schema`
pub const MANIFEST_SCHEMA: &str = "kairos-pulse.release/v1";
/// 版本字串的長度上限（bytes，同 hello 的 `agent_version`）
pub const MAX_VERSION_BYTES: usize = crate::MAX_AGENT_VERSION_BYTES;

// ---- 版本 ----

/// SemVer 2.0 的版本（`0.9.1`、`0.9.0-test.1`、`1.0.0+build.5`）。比較依 SemVer 的先後順序
/// （預發佈版本低於正式版本；build metadata 不影響比較與相等）
#[derive(Debug, Clone)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// 預發佈識別碼（`-` 之後以 `.` 分開的部分），沒有時為空
    pub pre: Vec<String>,
    /// build metadata（`+` 之後），沒有時 `None`
    pub build: Option<String>,
}

impl Version {
    /// 嚴格解析（數字不得有前導 0、識別碼只能是英數字與 `-`、最長 64 bytes）
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("invalid version {s:?}: {why}");
        if s.is_empty() || s.len() > MAX_VERSION_BYTES {
            return Err(bad("must be 1..=64 bytes"));
        }
        let (rest, build) = match s.split_once('+') {
            Some((r, b)) => (r, Some(b)),
            None => (s, None),
        };
        let (core, pre) = match rest.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (rest, None),
        };
        let numeric = |p: &str| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p == "0" || !p.starts_with('0'))
        };
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 || !parts.iter().all(|p| numeric(p)) {
            return Err(bad("expected MAJOR.MINOR.PATCH"));
        }
        let n = |p: &str| p.parse::<u64>().map_err(|_| bad("number too large"));
        let ident =
            |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        let pre: Vec<String> = match pre {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<&str> = p.split('.').collect();
                for id in &ids {
                    let all_digits = id.bytes().all(|b| b.is_ascii_digit());
                    if !ident(id) || (all_digits && !numeric(id)) {
                        return Err(bad("invalid pre-release identifier"));
                    }
                }
                ids.into_iter().map(str::to_string).collect()
            }
        };
        if let Some(b) = build
            && !b.split('.').all(ident)
        {
            return Err(bad("invalid build metadata"));
        }
        Ok(Version {
            major: n(parts[0])?,
            minor: n(parts[1])?,
            patch: n(parts[2])?,
            pre,
            build: build.map(str::to_string),
        })
    }

    /// `(MAJOR, MINOR, PATCH)`
    pub fn core(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }

    /// 是否為預發佈版本
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        if let Some(b) = &self.build {
            write!(f, "+{b}")?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core().cmp(&other.core()).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let ord = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

/// git commit 的縮寫：7～40 個小寫十六進位
pub fn is_commit(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 兩個 commit 的縮寫是否指同一個 commit：都合法（[`is_commit`]），而且其中一個是另一個的前綴。
/// 發佈檔名中的 commit 與執行檔內建的 `PULSE_BUILD_COMMIT` 長度可能不同（ADR-0003 方案 A2）
pub fn commits_match(a: &str, b: &str) -> bool {
    is_commit(a) && is_commit(b) && (a.starts_with(b) || b.starts_with(a))
}

// ---- 發佈檔名 ----

/// 產品
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Product {
    Kairos,
    Pulse,
}

impl Product {
    pub fn as_str(self) -> &'static str {
        match self {
            Product::Kairos => "kairos",
            Product::Pulse => "pulse",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "kairos" => Some(Product::Kairos),
            "pulse" => Some(Product::Pulse),
            _ => None,
        }
    }
}

/// 作業系統
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    pub fn as_str(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
        }
    }
}

/// CPU 架構（與 hello 的 `os.arch`、Rust 的 `std::env::consts::ARCH` 相同）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// 由 hello 的 `os.arch` 取得；不支援的架構為 `None`
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "x86_64" => Some(Arch::X86_64),
            "aarch64" => Some(Arch::Aarch64),
            _ => None,
        }
    }
}

/// 發佈檔的格式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    TarGz,
    Deb,
    Rpm,
    Msi,
    Zip,
    /// SBOM（CycloneDX）；不是可安裝的發佈檔
    CdxJson,
}

impl Format {
    /// 副檔名（不含開頭的 `.`）
    pub fn extension(self) -> &'static str {
        match self {
            Format::TarGz => "tar.gz",
            Format::Deb => "deb",
            Format::Rpm => "rpm",
            Format::Msi => "msi",
            Format::Zip => "zip",
            Format::CdxJson => "cdx.json",
        }
    }

    const ALL: [Format; 6] = [
        Format::TarGz,
        Format::Deb,
        Format::Rpm,
        Format::Msi,
        Format::Zip,
        Format::CdxJson,
    ];
}

/// 發佈檔名 `<名稱>-<版本>-<commit>[-dirty]-<os>-<arch>.<格式>`（RELEASING.md 第 1 節；
/// 例如 `pulse-0.9.1-5e6f7a8-linux-x86_64.deb`、`pulse-0.9.0-test.1-1a2b3c4-linux-x86_64.tar.gz`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseFile {
    pub product: Product,
    pub version: Version,
    pub commit: String,
    /// 從有未提交變更的工作目錄建置（不可簽章、不可作為正式發佈）
    pub dirty: bool,
    pub os: Os,
    pub arch: Arch,
    pub format: Format,
}

/// `<名稱>-<版本>-<commit>` 之後的部分（`-release.json` 或 `-<os>-<arch>.<格式>`）
fn split_product_version_commit(stem: &str) -> Result<(Product, Version, String, bool), String> {
    let (product, rest) = stem
        .split_once('-')
        .ok_or("expected <product>-<version>-<commit>")?;
    let product = Product::parse(product).ok_or("product must be kairos or pulse")?;
    let (rest, dirty) = match rest.strip_suffix("-dirty") {
        Some(r) => (r, true),
        None => (rest, false),
    };
    let (version, commit) = rest
        .rsplit_once('-')
        .ok_or("expected <product>-<version>-<commit>")?;
    if !is_commit(commit) {
        return Err("commit must be 7..=40 lowercase hex digits".to_string());
    }
    let version = Version::parse(version)?;
    Ok((product, version, commit.to_string(), dirty))
}

impl ReleaseFile {
    /// 解析發佈檔名（不接受路徑）
    pub fn parse(name: &str) -> Result<Self, String> {
        let err = |e: String| format!("invalid release file name {name:?}: {e}");
        if name.contains(['/', '\\']) || name.len() > 255 {
            return Err(err("must be a plain file name".to_string()));
        }
        let (stem, format) = Format::ALL
            .iter()
            .find_map(|f| {
                name.strip_suffix(f.extension())
                    .and_then(|s| s.strip_suffix('.'))
                    .map(|s| (s, *f))
            })
            .ok_or_else(|| err("unknown format".to_string()))?;
        let (rest, arch) = stem
            .rsplit_once('-')
            .ok_or_else(|| err("missing arch".to_string()))?;
        let arch = Arch::parse(arch).ok_or_else(|| err("arch must be x86_64 or aarch64".into()))?;
        let (rest, os) = rest
            .rsplit_once('-')
            .ok_or_else(|| err("missing os".to_string()))?;
        let os = match os {
            "linux" => Os::Linux,
            "windows" => Os::Windows,
            _ => return Err(err("os must be linux or windows".to_string())),
        };
        let (product, version, commit, dirty) = split_product_version_commit(rest).map_err(err)?;
        Ok(ReleaseFile {
            product,
            version,
            commit,
            dirty,
            os,
            arch,
            format,
        })
    }

    /// 解析清單檔名 `<名稱>-<版本>-<commit>-release.json`，回傳 (產品, 版本, commit)
    pub fn parse_manifest_name(name: &str) -> Result<(Product, Version, String), String> {
        let err = |e: String| format!("invalid manifest file name {name:?}: {e}");
        if name.contains(['/', '\\']) {
            return Err(err("must be a plain file name".to_string()));
        }
        let stem = name
            .strip_suffix("-release.json")
            .ok_or_else(|| err("must end with -release.json".to_string()))?;
        let (product, version, commit, dirty) = split_product_version_commit(stem).map_err(err)?;
        if dirty {
            return Err(err("a dirty build has no manifest".to_string()));
        }
        Ok((product, version, commit))
    }

    /// 清單檔名
    pub fn manifest_name(product: Product, version: &Version, commit: &str) -> String {
        format!("{}-{version}-{commit}-release.json", product.as_str())
    }

    /// 組回檔名
    pub fn file_name(&self) -> String {
        format!(
            "{}-{}-{}{}-{}-{}.{}",
            self.product.as_str(),
            self.version,
            self.commit,
            if self.dirty { "-dirty" } else { "" },
            self.os.as_str(),
            self.arch.as_str(),
            self.format.extension()
        )
    }
}

/// 依主機的 `install.method` 與 `os.arch` 選出發佈檔的平台與格式（REMOTE.md 第 2.4.1 節）：
/// `tar` → `linux-<arch>.tar.gz`、`deb` → `.deb`、`rpm` → `.rpm`、`msi` → `windows-x86_64.msi`、
/// `zip` → `windows-x86_64.zip`。`unknown`、不支援的架構為 `None`（推送中標為 `skipped`）
pub fn artifact_kind(install_method: &str, arch: &str) -> Option<(Os, Arch, Format)> {
    let arch = Arch::parse(arch)?;
    let (os, format) = match install_method {
        crate::mgmt::INSTALL_METHOD_TAR => (Os::Linux, Format::TarGz),
        crate::mgmt::INSTALL_METHOD_DEB => (Os::Linux, Format::Deb),
        crate::mgmt::INSTALL_METHOD_RPM => (Os::Linux, Format::Rpm),
        crate::mgmt::INSTALL_METHOD_MSI => (Os::Windows, Format::Msi),
        crate::mgmt::INSTALL_METHOD_ZIP => (Os::Windows, Format::Zip),
        _ => return None,
    };
    if os == Os::Windows && arch != Arch::X86_64 {
        return None;
    }
    Some((os, arch, format))
}

// ---- SHA256SUMS ----

/// `SHA256SUMS`：每行 `<64 個小寫十六進位>  <檔名>`（`sha256sum` 的文字模式輸出）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sha256Sums {
    entries: Vec<(String, String)>,
}

impl Sha256Sums {
    /// 嚴格解析：每行恰好是雜湊、兩個空白、檔名（不含 `/`、`\`、控制字元），以 `\n` 分行
    /// （最後一行可以沒有換行），不得有空行、`\r` 或重複的檔名
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut entries: Vec<(String, String)> = Vec::new();
        let body = text.strip_suffix('\n').unwrap_or(text);
        if body.is_empty() {
            return Err("SHA256SUMS is empty".to_string());
        }
        for (i, line) in body.split('\n').enumerate() {
            let bad = |why: &str| format!("SHA256SUMS line {}: {why}", i + 1);
            let (hash, name) = line
                .split_once("  ")
                .ok_or_else(|| bad("expected <sha256>  <file name>"))?;
            if !is_sha256_hex(hash) {
                return Err(bad("hash must be 64 lowercase hex digits"));
            }
            if name.is_empty()
                || name.starts_with([' ', '*'])
                || name.contains(['/', '\\'])
                || name.chars().any(char::is_control)
            {
                return Err(bad("invalid file name"));
            }
            if entries.iter().any(|(n, _)| n == name) {
                return Err(bad("duplicate file name"));
            }
            entries.push((name.to_string(), hash.to_string()));
        }
        Ok(Self { entries })
    }

    /// 檔名對應的雜湊（小寫十六進位）
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, h)| h.as_str())
    }

    /// 所有項目（檔名、雜湊），依檔案中的順序
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, h)| (n.as_str(), h.as_str()))
    }

    /// 檔案列在其中且雜湊相符
    pub fn check(&self, name: &str, sha256_hex: &str) -> Result<(), String> {
        match self.get(name) {
            None => Err(format!("{name} is not listed in SHA256SUMS")),
            Some(h) if h == sha256_hex => Ok(()),
            Some(_) => Err(format!("SHA-256 of {name} does not match SHA256SUMS")),
        }
    }
}

/// 資料的 SHA-256 小寫十六進位
pub fn sha256_hex(data: &[u8]) -> String {
    crate::enroll::sha256_hex(data)
}

// ---- 發佈清單 ----

/// 清單中的 systemd unit 雜湊（tar 安裝替換 unit 的判斷，REMOTE.md 第 4.4.1 節）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitHashes {
    /// 發佈包中 `pulse.service` 的 SHA-256
    pub sha256: String,
    /// 歷來官方 unit 的 SHA-256（目前的 unit 等於其中之一時表示客戶沒有改過）
    #[serde(default)]
    pub previous_sha256: Vec<String>,
}

/// 發佈清單 `pulse-<版本>-<commit>-release.json`（REMOTE.md 第 4.1.3 節；0.9.0 起，列在 `SHA256SUMS` 中）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// [`MANIFEST_SCHEMA`]
    pub schema: String,
    /// `pulse`（Kairos 的版本庫只接受 Pulse）
    pub product: String,
    pub version: String,
    pub commit: String,
    /// `YYYY-MM-DD`
    pub released_at: String,
    /// 這版需要的最低 Kairos（Kairos 低於它時不能建立推送，`KRS-W8415`）
    pub min_kairos: String,
    /// 能遠端升級到這版的最低 Pulse 版本（低於它的主機 `skipped`／`declined`）
    pub min_upgrade_from: String,
    /// 這版支援的遠端管理 feature（目標版本沒有 `upgrade` 時之後無法再遠端管理）
    #[serde(default)]
    pub remote_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<UnitHashes>,
    /// 有破壞性變更（主控台要求勾選「已閱讀升級步驟」）
    #[serde(default)]
    pub breaking: bool,
}

impl ReleaseManifest {
    /// 解析並檢查清單內容（≤ 8 KiB）
    pub fn parse(content: &str) -> Result<Self, String> {
        if content.len() > crate::mgmt::MAX_MANIFEST_BYTES {
            return Err(format!(
                "manifest must be at most {} bytes",
                crate::mgmt::MAX_MANIFEST_BYTES
            ));
        }
        let m: ReleaseManifest =
            serde_json::from_str(content).map_err(|e| format!("invalid manifest: {e}"))?;
        m.validate()?;
        Ok(m)
    }

    /// 欄位的限制
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(format!("manifest schema must be {MANIFEST_SCHEMA}"));
        }
        Product::parse(&self.product).ok_or("manifest product must be kairos or pulse")?;
        for (field, v) in [
            ("version", &self.version),
            ("min_kairos", &self.min_kairos),
            ("min_upgrade_from", &self.min_upgrade_from),
        ] {
            Version::parse(v).map_err(|e| format!("manifest {field}: {e}"))?;
        }
        if !is_commit(&self.commit) {
            return Err("manifest commit must be 7..=40 lowercase hex digits".to_string());
        }
        if chrono::NaiveDate::parse_from_str(&self.released_at, "%Y-%m-%d").is_err()
            || self.released_at.len() != 10
        {
            return Err("manifest released_at must be YYYY-MM-DD".to_string());
        }
        if self.remote_features.len() > crate::MAX_FEATURES {
            return Err("manifest remote_features has too many entries".to_string());
        }
        crate::validate_features(&self.remote_features)
            .map_err(|e| format!("manifest remote_features: {e}"))?;
        if let Some(u) = &self.unit
            && (!is_sha256_hex(&u.sha256) || !u.previous_sha256.iter().all(|h| is_sha256_hex(h)))
        {
            return Err("manifest unit hashes must be 64 lowercase hex digits".to_string());
        }
        Ok(())
    }

    /// 產品
    pub fn product(&self) -> Product {
        Product::parse(&self.product).unwrap_or(Product::Pulse)
    }

    /// 版本（[`ReleaseManifest::validate`] 通過後一定可以解析）
    pub fn version(&self) -> Version {
        Version::parse(&self.version).unwrap_or_else(|_| Version::parse("0.0.0").unwrap())
    }

    /// 清單應有的檔名
    pub fn file_name(&self) -> String {
        ReleaseFile::manifest_name(self.product(), &self.version(), &self.commit)
    }

    /// 這版支援遠端升級（降到沒有 `upgrade` 的版本時，主控台要求確認「之後無法再遠端管理」）
    pub fn supports_remote_upgrade(&self) -> bool {
        self.remote_features
            .iter()
            .any(|f| f == crate::mgmt::FEATURE_UPGRADE)
    }

    /// 目前的 Kairos 版本是否符合 `min_kairos`（REMOTE.md 第 4.9 節；不符時 `KRS-W8415`）
    pub fn kairos_compatible(&self, kairos_version: &Version) -> bool {
        Version::parse(&self.min_kairos).is_ok_and(|min| *kairos_version >= min)
    }
}

// ---- 驗證流程 ----

/// 驗證失敗：簽章（`PLS-E8401`／`KRS-E8401`）或雜湊、檔名、平台、版本（`PLS-E8402`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseError {
    Signature(SigError),
    Integrity(String),
}

impl ReleaseError {
    /// 產品代碼的號碼（REMOTE.md 第 11 節）：簽章 8401、其他 8402
    pub fn code_number(&self) -> u16 {
        match self {
            ReleaseError::Signature(_) => 8401,
            ReleaseError::Integrity(_) => 8402,
        }
    }
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReleaseError::Signature(e) => write!(f, "release signature verification failed: {e}"),
            ReleaseError::Integrity(e) => write!(f, "release integrity check failed: {e}"),
        }
    }
}

impl std::error::Error for ReleaseError {}

/// 信任設定（Pulse／輔助程式：本機 `allowed_signers` 與 `upgrade.extra_principals`；
/// Kairos：`[remote] allowed_signers`）
#[derive(Debug, Clone, Copy)]
pub struct TrustPolicy<'a> {
    pub signers: &'a AllowedSigners,
    /// 除 [`RELEASE_PRINCIPAL`] 外另外接受的 principal（客戶自行重新簽署，REMOTE.md 第 14 節 D14）
    pub extra_principals: &'a [String],
    /// 目前的 Unix 秒（`valid-after`／`valid-before` 的判斷）
    pub now_unix: i64,
}

/// 一次發佈的簽章資料
#[derive(Debug, Clone, Copy)]
pub struct ReleaseBundle<'a> {
    /// `SHA256SUMS` 全文
    pub sha256sums: &'a str,
    /// `SHA256SUMS.sig` 全文
    pub signature: &'a str,
    /// 發佈清單（檔名、內容）；0.9.0 以前的發佈沒有
    pub manifest: Option<(&'a str, &'a str)>,
}

/// [`verify_bundle`] 通過後的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBundle {
    pub signer: VerifiedSigner,
    pub sums: Sha256Sums,
    pub manifest: Option<ReleaseManifest>,
}

/// REMOTE.md 第 4.2.2 節的步驟 1～3：以信任錨驗證 `SHA256SUMS.sig`，解析 `SHA256SUMS`，
/// 確認清單列在其中、雜湊相符、內容合法且檔名與內容一致
pub fn verify_bundle(
    bundle: &ReleaseBundle<'_>,
    trust: &TrustPolicy<'_>,
) -> Result<VerifiedBundle, ReleaseError> {
    let principals: Vec<&str> = std::iter::once(RELEASE_PRINCIPAL)
        .chain(trust.extra_principals.iter().map(String::as_str))
        .collect();
    let signer = verify_sshsig(
        trust.signers,
        &principals,
        RELEASE_NAMESPACE,
        bundle.sha256sums.as_bytes(),
        bundle.signature,
        trust.now_unix,
    )
    .map_err(ReleaseError::Signature)?;
    let sums = Sha256Sums::parse(bundle.sha256sums).map_err(ReleaseError::Integrity)?;
    let manifest = match bundle.manifest {
        None => None,
        Some((name, content)) => {
            sums.check(name, &sha256_hex(content.as_bytes()))
                .map_err(ReleaseError::Integrity)?;
            let m = ReleaseManifest::parse(content).map_err(ReleaseError::Integrity)?;
            if m.file_name() != name {
                return Err(ReleaseError::Integrity(format!(
                    "manifest file name {name} does not match its content ({})",
                    m.file_name()
                )));
            }
            Some(m)
        }
    };
    Ok(VerifiedBundle {
        signer,
        sums,
        manifest,
    })
}

/// [`VerifiedBundle::check_artifact`] 的期望值；`None` 的欄位不檢查
#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactExpectation<'a> {
    pub product: Option<Product>,
    /// 提議的 `target.version`
    pub version: Option<&'a str>,
    /// 提議的 `target.commit`
    pub commit: Option<&'a str>,
    pub os: Option<Os>,
    pub arch: Option<Arch>,
    pub format: Option<Format>,
}

impl VerifiedBundle {
    /// REMOTE.md 第 4.2.2 節的步驟 3～4：發佈檔列在 `SHA256SUMS` 中且雜湊相符，檔名合法、不是 `-dirty`，
    /// 版本與 commit 與清單（有的話）和期望值一致，平台與格式符合
    pub fn check_artifact(
        &self,
        filename: &str,
        sha256_hex: &str,
        expect: &ArtifactExpectation<'_>,
    ) -> Result<ReleaseFile, ReleaseError> {
        let integrity = |e: String| ReleaseError::Integrity(e);
        self.sums.check(filename, sha256_hex).map_err(integrity)?;
        let file = ReleaseFile::parse(filename).map_err(integrity)?;
        if file.dirty {
            return Err(integrity(format!("{filename} is a dirty build")));
        }
        if file.format == Format::CdxJson {
            return Err(integrity(format!(
                "{filename} is not an installable artifact"
            )));
        }
        if let Some(m) = &self.manifest
            && (file.version != m.version()
                || file.commit != m.commit
                || file.product != m.product())
        {
            return Err(integrity(format!(
                "{filename} does not match the manifest ({} {} {})",
                m.product, m.version, m.commit
            )));
        }
        let mismatch = |what: &str| integrity(format!("{filename}: {what} does not match"));
        if expect.product.is_some_and(|p| p != file.product) {
            return Err(mismatch("product"));
        }
        if let Some(v) = expect.version
            && !Version::parse(v).is_ok_and(|v| v == file.version && v.build.is_none())
        {
            return Err(mismatch("version"));
        }
        if expect.commit.is_some_and(|c| c != file.commit) {
            return Err(mismatch("commit"));
        }
        if expect.os.is_some_and(|o| o != file.os) {
            return Err(mismatch("os"));
        }
        if expect.arch.is_some_and(|a| a != file.arch) {
            return Err(mismatch("arch"));
        }
        if expect.format.is_some_and(|f| f != file.format) {
            return Err(mismatch("format"));
        }
        Ok(file)
    }
}

/// [`check_version_policy`] 的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionAction {
    /// 目標版本較新
    Upgrade,
    /// 相同版本（重新安裝；不視為降版）
    Reinstall,
    /// 目標版本較舊（推送與本機都允許）
    Downgrade,
}

/// 依版本政策拒絕（`declined`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecline {
    /// 降版，但推送或本機不允許（`PLS-W8410`）
    DowngradeNotAllowed,
    /// 目前版本低於清單的 `min_upgrade_from`，需要手動升級（`detail: min_upgrade_from`）
    BelowMinUpgradeFrom { min_upgrade_from: String },
    /// 升級的目標沒有發佈清單（0.9.0 以前的發佈只能作為降版或回滾目標）
    MissingManifest,
}

impl PolicyDecline {
    /// 給 `upgrade_status.detail` 用的機器可讀原因
    pub fn detail(&self) -> &'static str {
        match self {
            PolicyDecline::DowngradeNotAllowed => "downgrade_not_allowed",
            PolicyDecline::BelowMinUpgradeFrom { .. } => "min_upgrade_from",
            PolicyDecline::MissingManifest => "no_manifest",
        }
    }
}

/// REMOTE.md 第 4.2.2 節的步驟 5 與第 4.8 節：
/// - 目標較新：需要清單，且目前版本不低於清單的 `min_upgrade_from`；
/// - 相同版本：重新安裝（不視為降版）；
/// - 目標較舊：推送的 `allow_downgrade` 與本機的 `upgrade.allow_downgrade` 都為 `true` 才接受
///   （`min_upgrade_from` 不適用）。自動回滾是本機的動作，不經過這個判斷
pub fn check_version_policy(
    current: &Version,
    target: &Version,
    manifest: Option<&ReleaseManifest>,
    offer_allows_downgrade: bool,
    local_allows_downgrade: bool,
) -> Result<VersionAction, PolicyDecline> {
    match target.cmp(current) {
        Ordering::Equal => Ok(VersionAction::Reinstall),
        Ordering::Less => {
            if offer_allows_downgrade && local_allows_downgrade {
                Ok(VersionAction::Downgrade)
            } else {
                Err(PolicyDecline::DowngradeNotAllowed)
            }
        }
        Ordering::Greater => {
            let m = manifest.ok_or(PolicyDecline::MissingManifest)?;
            let min = Version::parse(&m.min_upgrade_from).map_err(|_| {
                PolicyDecline::BelowMinUpgradeFrom {
                    min_upgrade_from: m.min_upgrade_from.clone(),
                }
            })?;
            if *current < min {
                Err(PolicyDecline::BelowMinUpgradeFrom {
                    min_upgrade_from: m.min_upgrade_from.clone(),
                })
            } else {
                Ok(VersionAction::Upgrade)
            }
        }
    }
}

// ---- 已驗證發佈的本機紀錄（ADR-0003 第 7.1 節 Q3） ----

/// [`ReleaseRecord::schema`]
pub const RELEASE_RECORD_SCHEMA: &str = "kairos-pulse.release-record/v1";
/// [`ReleaseRecord::source`]：遠端升級（OTA）成功、健康確認通過後由特權元件封存
pub const RECORD_SOURCE_OTA: &str = "ota";
/// [`ReleaseRecord::source`]：安裝時由一行安裝腳本交給特權元件驗證並封存的種子
pub const RECORD_SOURCE_SEED: &str = "seed";
/// [`ReleaseRecord::source`]：本機管理者事後以命令（`pulse upgrade archive-current`）封存目前安裝的版本
pub const RECORD_SOURCE_INSTALL: &str = "install";

/// 驗證當時的信任政策快照（[`ReleaseRecord::policy`]）：之後判斷「簽署金鑰是否被撤銷」以指紋比對
/// 目前的信任錨；其餘欄位供鑑識
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustSnapshot {
    /// 簽署金鑰的指紋（`SHA256:…`）
    pub signer: String,
    /// 符合的 principal
    #[serde(default)]
    pub principal: String,
    /// 驗證當時 `allowed_signers` 那一行的 `valid-after`／`valid-before`（Unix 秒；沒有時 `None`）
    #[serde(default)]
    pub valid_after: Option<i64>,
    #[serde(default)]
    pub valid_before: Option<i64>,
    /// 驗證當時信任錨檔案的 SHA-256
    #[serde(default)]
    pub anchor_sha256: String,
    /// 驗證的時間（Unix 秒）
    #[serde(default)]
    pub verified_at: i64,
}

impl TrustSnapshot {
    /// 由驗證結果與當時的信任錨組成
    pub fn of(
        signer: &VerifiedSigner,
        signers: &AllowedSigners,
        anchor_sha256: &str,
        now_unix: i64,
    ) -> Self {
        let entry = signers.entries.iter().find(|e| e.line == signer.line);
        Self {
            signer: signer.fingerprint.clone(),
            principal: signer.principal.clone(),
            valid_after: entry.and_then(|e| e.valid_after),
            valid_before: entry.and_then(|e| e.valid_before),
            anchor_sha256: anchor_sha256.to_string(),
            verified_at: now_unix,
        }
    }
}

/// 一份「已驗證發佈」的本機紀錄：Linux 升級輔助程式的本機封存（`/var/lib/pulse-upgrade/archive/`，REMOTE.md
/// 第 17.10 節）的條目；Windows 的版本槽紀錄（ADR-0001）沿用同一種內容。**只定義內容的格式**；信任來自
/// 它存放的位置（只有特權元件可寫）與提交紀錄，不來自這份資料本身（INV-6、INV-10）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRecord {
    /// [`RELEASE_RECORD_SCHEMA`]
    pub schema: String,
    /// **主鍵**：發佈檔（套件）整包的 SHA-256
    pub artifact_sha256: String,
    /// 索引與人讀資訊
    pub version: String,
    pub commit: String,
    /// 發佈時的檔名
    pub artifact: String,
    /// 發佈檔中代理程式執行檔的 SHA-256（回滾時與目前安裝的執行檔比對）
    pub binary_sha256: String,
    /// [`RECORD_SOURCE_OTA`]／[`RECORD_SOURCE_SEED`]／[`RECORD_SOURCE_INSTALL`]
    pub source: String,
    /// 驗證當時的信任政策
    pub policy: TrustSnapshot,
    /// 偵測到簽署金鑰已被撤銷（不在信任錨中）的時間（Unix 秒）；沒有偵測到時 `None`
    #[serde(default)]
    pub revoked_at: Option<i64>,
    /// 這個條目被提交時的 generation（單調遞增；只有提交紀錄中的條目才是回滾目標）
    #[serde(default)]
    pub generation: u64,
    /// 封存的時間（RFC 3339）
    #[serde(default)]
    pub archived_at: String,
}

impl ReleaseRecord {
    /// 內容的基本檢查：schema、雜湊的格式、版本、commit 與檔名一致
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != RELEASE_RECORD_SCHEMA {
            return Err(format!("unexpected schema {:?}", self.schema));
        }
        if !is_sha256_hex(&self.artifact_sha256) || !is_sha256_hex(&self.binary_sha256) {
            return Err("artifact_sha256 and binary_sha256 must be SHA-256 hex".to_string());
        }
        let file = ReleaseFile::parse(&self.artifact)?;
        let version = Version::parse(&self.version)?;
        if file.version != version || file.commit != self.commit || file.dirty {
            return Err(format!(
                "{} does not match version {} commit {}",
                self.artifact, self.version, self.commit
            ));
        }
        if self.policy.signer.is_empty() {
            return Err("policy.signer must not be empty".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
