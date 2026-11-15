//! root 專屬的本機封存（ADR-0003 階段 2、方案 B；REMOTE.md 第 17.10 節）。
//!
//! 升級成功（健康確認通過）後，輔助程式把剛裝好、已驗證的套件連同它的簽章資料封存到
//! `/var/lib/pulse-upgrade/archive/<發佈檔 SHA-256>/`（root 0700），並以**一次原子寫入的索引**
//! （`archive/index.json`，有 checksum 與單調遞增的 generation）提交。下一次升級時，回滾來源優先使用
//! 封存：它是這台主機實際安裝過的那一份完整封裝，不依賴 Kairos、外部 URL 或服務帳號可寫的任何檔案。
//!
//! - **只有索引中的條目是回滾目標**（INV-10）：封存目錄內的 `record.json` 只是快取；不在索引中的目錄
//!   （封存到一半、已被取代）一律不使用，下一次提交時清除。
//! - 保留 [`KEEP`] 份（目前安裝的版本）。交易使用的回滾套件在替換前已複製到那筆交易的工作目錄，所以
//!   清除舊條目不會影響進行中的交易。
//! - 所有檔案以「寫暫存檔 → fsync → 改名 → fsync 目錄」寫入；讀取時不跟隨符號連結、只接受一般檔案，
//!   目錄必須由 root 擁有、群組與其他人沒有任何權限（INV-6）。

use std::path::{Path, PathBuf};

use kairos_proto::release::{self, AllowedSigners, RELEASE_RECORD_SCHEMA, ReleaseRecord};
use serde::{Deserialize, Serialize};

use super::txn::{self, Owner};
use crate::remote::upgrade::files;

/// 封存的根目錄（在輔助程式的工作目錄之下）
pub const ARCHIVE_DIR: &str = "archive";
/// 提交紀錄
pub const INDEX_FILE: &str = "index.json";
pub const INDEX_SCHEMA: &str = "pulse.upgrade-archive/v1";
/// 條目目錄中的檔案
pub const PACKAGE_FILE: &str = "package";
pub const RECORD_FILE: &str = "record.json";
/// 保留的份數（ADR-0003 第 11 節 P3：目前安裝的版本）
pub const KEEP: usize = 1;
/// 封存前在套件大小之外另外要求的可用空間（bytes）
pub const SPACE_MARGIN: u64 = 16 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 256 * 1024;
/// 封存到一半的暫存目錄的前綴
const STAGING_PREFIX: &str = ".staging-";

/// 提交紀錄（`archive/index.json`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub schema: String,
    pub checksum: String,
    /// 每次提交加一
    pub generation: u64,
    #[serde(default)]
    pub updated_at: String,
    pub entries: Vec<ReleaseRecord>,
}

impl Index {
    fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut i = self.clone();
        i.checksum = txn::CHECKSUM_PLACEHOLDER.to_string();
        let text =
            serde_json::to_string_pretty(&i).map_err(|e| format!("無法產生封存索引：{e}"))?;
        txn::seal(&text).map(String::into_bytes)
    }
}

/// 封存的根目錄
pub fn dir(work_root: &Path) -> PathBuf {
    work_root.join(ARCHIVE_DIR)
}

/// 條目的目錄
pub fn entry_dir(work_root: &Path, artifact_sha256: &str) -> PathBuf {
    dir(work_root).join(artifact_sha256)
}

fn exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: cannot be examined: {e}", path.display())),
    }
}

/// 讀取索引。`Ok(None)`：沒有封存（目錄或索引不存在）；`Err`：存在但不能用——目錄不安全、檔案讀不到
/// （含符號連結、硬連結）、不是合法的內容、checksum 不符、schema 不認得或條目不合規定
pub fn read_index(work_root: &Path, owner: Owner) -> Result<Option<Index>, String> {
    let dir = dir(work_root);
    if !exists(&dir)? {
        return Ok(None);
    }
    // 既存的目錄同樣檢查（只多了讀取／執行權限時改回 0700）
    txn::ensure_private_dir(&dir, owner)?;
    let path = dir.join(INDEX_FILE);
    if !exists(&path)? {
        return Ok(None);
    }
    let bytes = files::read_untrusted_file(&path, MAX_INDEX_BYTES)
        .map_err(|e| format!("{INDEX_FILE} cannot be read: {e}"))?;
    let index: Index =
        serde_json::from_slice(&bytes).map_err(|e| format!("{INDEX_FILE} is not valid: {e}"))?;
    if index.schema != INDEX_SCHEMA {
        return Err(format!("{INDEX_FILE} has schema {:?}", index.schema));
    }
    txn::verify_seal(&bytes).map_err(|e| format!("{INDEX_FILE}: {e}"))?;
    for e in &index.entries {
        e.validate()
            .map_err(|err| format!("{INDEX_FILE}: entry {}: {err}", e.artifact_sha256))?;
        if e.generation > index.generation {
            return Err(format!(
                "{INDEX_FILE}: entry {} has a generation newer than the index",
                e.artifact_sha256
            ));
        }
    }
    Ok(Some(index))
}

/// 索引中 `version`、`commit`（前綴比對）的條目；有多筆時取 generation 最大的
pub fn find<'a>(index: &'a Index, version: &str, commit: &str) -> Option<&'a ReleaseRecord> {
    index
        .entries
        .iter()
        .filter(|e| e.version == version && release::commits_match(&e.commit, commit))
        .max_by_key(|e| e.generation)
}

/// [`lookup`] 的結果
#[derive(Debug)]
pub enum Lookup {
    /// 沒有封存，或沒有這個版本的條目（不算不合格：評估下一個來源，不記原因）
    None,
    /// 合格：條目與封存的套件
    Usable(Box<ReleaseRecord>, PathBuf),
    /// 有這個版本的條目（或索引本身）但不合格：降級回滾的原因代碼與給日誌的說明
    Unusable(&'static str, String),
}

/// 回滾來源第 1 項的資格（ADR-0003 第 5.4 節；全部成立才使用）：索引可用、有一筆已提交的條目是
/// `version`／`commit`（輔助程式自己的，INV-1）、條目記錄的執行檔雜湊等於 `binary_sha256`（目前安裝的
/// 執行檔，INV-2）、封存的套件整包雜湊等於條目的主鍵、簽署金鑰仍列在信任錨中（到期可以，撤銷不行，
/// INV-7）
pub fn lookup(
    work_root: &Path,
    owner: Owner,
    version: &str,
    commit: &str,
    binary_sha256: &str,
    signers: &AllowedSigners,
) -> Lookup {
    use kairos_proto::mgmt::{
        DEGRADED_ARCHIVE_UNUSABLE, DEGRADED_BINARY_MISMATCH, DEGRADED_SIGNER_REVOKED,
    };
    let index = match read_index(work_root, owner) {
        Ok(None) => return Lookup::None,
        Ok(Some(i)) => i,
        Err(e) => return Lookup::Unusable(DEGRADED_ARCHIVE_UNUSABLE, e),
    };
    let Some(rec) = find(&index, version, commit) else {
        return Lookup::None;
    };
    if rec.binary_sha256 != binary_sha256 {
        return Lookup::Unusable(
            DEGRADED_BINARY_MISMATCH,
            format!("封存的 {} 記錄的執行檔與目前安裝的不同", rec.artifact),
        );
    }
    let entry = entry_dir(work_root, &rec.artifact_sha256);
    match exists(&entry) {
        Ok(true) => {}
        Ok(false) => {
            return Lookup::Unusable(
                DEGRADED_ARCHIVE_UNUSABLE,
                format!("{} is missing", entry.display()),
            );
        }
        Err(e) => return Lookup::Unusable(DEGRADED_ARCHIVE_UNUSABLE, e),
    }
    if let Err(e) = txn::ensure_private_dir(&entry, owner) {
        return Lookup::Unusable(DEGRADED_ARCHIVE_UNUSABLE, e);
    }
    let package = entry.join(PACKAGE_FILE);
    if txn::sha256_regular(&package).as_deref() != Some(rec.artifact_sha256.as_str()) {
        return Lookup::Unusable(
            DEGRADED_ARCHIVE_UNUSABLE,
            format!(
                "{} is missing or does not match its recorded SHA-256",
                package.display()
            ),
        );
    }
    if !signers.lists_fingerprint(&rec.policy.signer) {
        // 記下偵測到撤銷的時間（鑑識用；盡力而為）
        mark_revoked(
            work_root,
            owner,
            &rec.artifact_sha256,
            chrono::Utc::now().timestamp(),
        );
        return Lookup::Unusable(
            DEGRADED_SIGNER_REVOKED,
            format!(
                "封存的 {} 的簽署金鑰 {} 已不在信任錨中（已撤銷）",
                rec.artifact, rec.policy.signer
            ),
        );
    }
    Lookup::Usable(Box::new(rec.clone()), package)
}

/// 要封存的一份發佈
pub struct NewEntry<'a> {
    /// `generation`、`archived_at` 由 [`commit`] 填入
    pub record: ReleaseRecord,
    /// 已驗證的套件（root 專用位置中的複本）
    pub package: &'a Path,
    /// 它所屬發佈的 `SHA256SUMS` 與簽章
    pub sums: &'a str,
    pub signature: &'a str,
    /// 發佈清單（檔名、內容）；沒有時 `None`
    pub manifest: Option<(&'a str, &'a str)>,
}

/// [`commit`] 的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Committed {
    /// 已提交：新的 generation 與被清除的舊條目
    New {
        generation: u64,
        pruned: Vec<String>,
    },
    /// 索引已經只有這一筆、封存的套件也完好（上一次在提交之後中斷）：沒有變更
    AlreadyCurrent { generation: u64 },
}

/// 封存並提交（可以重做）：
///
/// 1. 檢查大小與可用空間（套件大小加 [`SPACE_MARGIN`]）；
/// 2. 在暫存目錄寫好套件（複製後以主鍵核對雜湊）、簽章資料與 `record.json`，fsync 後改名為
///    `<發佈檔 SHA-256>/`——checkpoint `archive_staged`：此時索引還沒有它，不是回滾目標；
/// 3. 寫入新的索引（generation 加一，條目只留這一筆）——**這一次原子寫入就是提交**；checkpoint
///    `archive_committed`；
/// 4. 清除不在索引中的目錄。
///
/// 任何一步失敗都回錯誤，索引維持原狀（呼叫端只記錄：封存失敗不影響升級的結果）
pub fn commit(
    work_root: &Path,
    owner: Owner,
    entry: NewEntry<'_>,
    free_space: fn(&Path) -> Option<u64>,
    checkpoint: &dyn Fn(&str),
) -> Result<Committed, String> {
    let mut record = entry.record;
    record.schema = RELEASE_RECORD_SCHEMA.to_string();
    record.validate()?;
    let dir = dir(work_root);
    txn::ensure_private_dir(&dir, owner)?;
    // 索引讀不到（損毀）時不阻擋新的提交：新的索引取代它，generation 重新起算在它之後無法得知的值——
    // 以 0 為前值。舊的條目本來就不能用了
    let previous = read_index(work_root, owner).unwrap_or(None);
    let generation = previous.as_ref().map_or(0, |i| i.generation);
    let sha = record.artifact_sha256.clone();
    let final_dir = dir.join(&sha);
    let final_pkg = final_dir.join(PACKAGE_FILE);
    if let Some(i) = &previous
        && i.entries.len() == 1
        && i.entries[0].artifact_sha256 == sha
        && txn::sha256_regular(&final_pkg).as_deref() == Some(sha.as_str())
    {
        prune(work_root, i);
        return Ok(Committed::AlreadyCurrent { generation });
    }
    let size = std::fs::symlink_metadata(entry.package)
        .map_err(|e| format!("{}: cannot be examined: {e}", entry.package.display()))?
        .len();
    if size == 0 || size > files::MAX_FILE_BYTES {
        return Err(format!(
            "package size {size} is outside 1..={}",
            files::MAX_FILE_BYTES
        ));
    }
    let need = size.saturating_add(SPACE_MARGIN);
    if let Some(free) = free_space(&dir)
        && free < need
    {
        return Err(format!(
            "insufficient_space: need {need} bytes, free {free} bytes"
        ));
    }
    // 2. 暫存目錄 → 改名
    let staging = dir.join(format!("{STAGING_PREFIX}{sha}"));
    let _ = std::fs::remove_dir_all(&staging);
    let staged = (|| -> Result<(), String> {
        txn::ensure_private_dir(&staging, owner)?;
        let pkg = staging.join(PACKAGE_FILE);
        txn::copy_durable(entry.package, &pkg, 0o600)
            .map_err(|e| format!("cannot copy the package: {e}"))?;
        if txn::sha256_regular(&pkg).as_deref() != Some(sha.as_str()) {
            return Err("the copied package does not match artifact_sha256".to_string());
        }
        let write = |name: &str, bytes: &[u8]| {
            crate::remote::write_atomic_durable(&staging.join(name), bytes, 0o600, 0o700)
                .map_err(|e| format!("cannot write {name}: {e}"))
        };
        write(files::SUMS_FILE, entry.sums.as_bytes())?;
        write(files::SIG_FILE, entry.signature.as_bytes())?;
        if let Some((name, content)) = entry.manifest
            && kairos_proto::release::ReleaseFile::parse_manifest_name(name).is_ok()
        {
            write(name, content.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let generation = generation + 1;
    record.generation = generation;
    record.archived_at = crate::protocol::now_ts();
    let finish = (|| -> Result<(), String> {
        let cache = serde_json::to_vec_pretty(&record)
            .map_err(|e| format!("cannot encode {RECORD_FILE}: {e}"))?;
        crate::remote::write_atomic_durable(&staging.join(RECORD_FILE), &cache, 0o600, 0o700)
            .map_err(|e| format!("cannot write {RECORD_FILE}: {e}"))?;
        txn::fsync_dir(&staging).map_err(|e| format!("cannot sync the staging directory: {e}"))?;
        if exists(&final_dir)? {
            // 同一個主鍵的舊目錄（內容不完整或被動過）：整個換掉
            std::fs::remove_dir_all(&final_dir)
                .map_err(|e| format!("cannot replace {}: {e}", final_dir.display()))?;
        }
        std::fs::rename(&staging, &final_dir)
            .map_err(|e| format!("cannot rename the staging directory: {e}"))?;
        txn::fsync_dir(&dir).map_err(|e| format!("cannot sync {}: {e}", dir.display()))
    })();
    if let Err(e) = finish {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    checkpoint("archive_staged");
    // 3. 提交
    let index = Index {
        schema: INDEX_SCHEMA.to_string(),
        checksum: String::new(),
        generation,
        updated_at: crate::protocol::now_ts(),
        entries: vec![record],
    };
    debug_assert!(index.entries.len() <= KEEP);
    let bytes = index.to_bytes()?;
    let path = dir.join(INDEX_FILE);
    crate::remote::write_atomic_durable(&path, &bytes, 0o600, 0o700)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    checkpoint("archive_committed");
    // 4. 清除
    let pruned = prune(work_root, &index);
    Ok(Committed::New { generation, pruned })
}

/// 清除封存目錄中不在索引裡的項目（被取代的條目、封存到一半的暫存目錄）。只處理名稱是 64 個十六進位
/// 字元或暫存前綴的**目錄**；其他名稱不動。回傳清除的名稱
pub fn prune(work_root: &Path, index: &Index) -> Vec<String> {
    let dir = dir(work_root);
    let mut removed = Vec::new();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let ours = kairos_proto::mgmt::is_sha256_hex(&name) || name.starts_with(STAGING_PREFIX);
        if !ours || index.entries.iter().any(|x| x.artifact_sha256 == name) {
            continue;
        }
        let is_dir = std::fs::symlink_metadata(e.path()).is_ok_and(|m| m.file_type().is_dir());
        if is_dir && std::fs::remove_dir_all(e.path()).is_ok() {
            removed.push(name);
        }
    }
    if !removed.is_empty() {
        let _ = txn::fsync_dir(&dir);
    }
    removed.sort();
    removed
}

/// 記下「偵測到簽署金鑰已撤銷」的時間（鑑識用；盡力而為，失敗不影響流程）。generation 不變：這不是
/// 新的提交，條目仍在索引中、只是之後的 [`lookup`] 照樣會因為撤銷而不使用它
pub fn mark_revoked(work_root: &Path, owner: Owner, artifact_sha256: &str, now_unix: i64) {
    let Ok(Some(mut index)) = read_index(work_root, owner) else {
        return;
    };
    let mut changed = false;
    for e in &mut index.entries {
        if e.artifact_sha256 == artifact_sha256 && e.revoked_at.is_none() {
            e.revoked_at = Some(now_unix);
            changed = true;
        }
    }
    if !changed {
        return;
    }
    index.updated_at = crate::protocol::now_ts();
    if let Ok(bytes) = index.to_bytes() {
        let _ = crate::remote::write_atomic_durable(
            &dir(work_root).join(INDEX_FILE),
            &bytes,
            0o600,
            0o700,
        );
    }
}

/// 目前安裝的執行檔（`binary_sha256`）是否有可用的封存：索引可用、有條目的執行檔雜湊相符、封存的套件
/// 完好。給 hello 的 `install.rollback` 用的提示（[`super::rollback_hint`]）；真正選用時仍走 [`lookup`]
pub fn has_entry_for_binary(work_root: &Path, owner: Owner, binary_sha256: &str) -> bool {
    let Ok(Some(index)) = read_index(work_root, owner) else {
        return false;
    };
    index.entries.iter().any(|e| {
        e.binary_sha256 == binary_sha256
            && txn::sha256_regular(&entry_dir(work_root, &e.artifact_sha256).join(PACKAGE_FILE))
                .as_deref()
                == Some(e.artifact_sha256.as_str())
    })
}
