//! 輔助程式的交易紀錄（`crates/kairos-proto/REMOTE.md` 第 17.9 節；ADR-0003 階段 0b）：
//!
//! - [`Record`]：`/var/lib/pulse-upgrade/<upgrade_id>/state.json`（schema v2，含 checksum 與綁定的雜湊）；
//!   [`LegacyState`] 是 0.9.0 的 v1。
//! - [`Active`]：進行中交易的指標 `/var/lib/pulse-upgrade/active.json`。
//! - 路徑檢查（[`check_install`]、[`ensure_private_dir`]）、工作目錄的掃描與清理（[`scan`]、[`prune`]）、
//!   耐久的檔案寫入（寫暫存檔 → fsync → 改名 → fsync 目錄）。
//!
//! 這裡只有資料與檔案操作，流程在上層的 `helper.rs`。

use std::io::Read;
use std::path::{Path, PathBuf};

use kairos_proto::mgmt::UpgradeTarget;
use serde::{Deserialize, Serialize};

use super::super::files::{self, UpgradeResult};

/// 交易紀錄的 schema
pub const RECORD_SCHEMA: &str = "pulse.upgrade-helper/v2";
/// 0.9.0 的狀態檔（沒有指標、沒有雜湊）
pub const LEGACY_SCHEMA: &str = "pulse.upgrade-helper/v1";
/// 進行中交易的指標的 schema
pub const ACTIVE_SCHEMA: &str = "pulse.upgrade-active/v1";
/// 進行中交易的指標（工作目錄的根目錄下）
pub const ACTIVE_FILE: &str = "active.json";
/// 交易紀錄的檔名（工作目錄中）
pub const STATE_FILE: &str = "state.json";
/// 日誌的檔名（工作目錄中）
pub const LOG_FILE: &str = "upgrade.log";
/// 工作目錄中的檔名：新的執行檔（tar）、新舊 unit（tar）、驗證過的回滾套件（deb／rpm）
pub const NEW_BINARY_FILE: &str = "new.bin";
pub const NEW_UNIT_FILE: &str = "pulse.service.new";
pub const UNIT_BACKUP_FILE: &str = "pulse.service.prev";
pub const ROLLBACK_PKG_FILE: &str = "rollback.pkg";
/// 紀錄與指標的大小上限
const MAX_RECORD_BYTES: u64 = 256 * 1024;
/// 保留的 `rollback_failed` 紀錄數（第 17.9.8 節）
pub const KEEP_ROLLBACK_FAILED: usize = 3;
/// checksum 計算時代入的值
const CHECKSUM_PLACEHOLDER: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
const CHECKSUM_KEY: &str = "\"checksum\": \"";

/// 交易的步驟（第 17.9.3 節）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// 紀錄已有替換前的狀態；複製、驗證、預檢中。系統沒有變更
    Preparing,
    /// 已驗證的複本都在工作目錄、雜湊已記錄；系統沒有變更
    Prepared,
    /// 指標已寫入：替換中
    Replacing,
    /// 已替換；健康確認中
    Replaced,
    /// 已記錄回滾的意圖；還原中
    RollbackIntent,
    /// 已還原；確認舊版恢復中
    RollingBack,
    /// 完成（結果已記錄）
    Done,
}

impl Step {
    /// 這個步驟代表系統可能已被變更、交易尚未結束（必須有指標）
    pub fn unfinished(self) -> bool {
        matches!(
            self,
            Step::Replacing | Step::Replaced | Step::RollbackIntent | Step::RollingBack
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            Step::Preparing => "preparing",
            Step::Prepared => "prepared",
            Step::Replacing => "replacing",
            Step::Replaced => "replaced",
            Step::RollbackIntent => "rollback_intent",
            Step::RollingBack => "rolling_back",
            Step::Done => "done",
        }
    }
}

/// 驗證過的檔案（發佈檔、回滾套件）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    /// 發佈時的檔名
    pub name: String,
    /// SHA-256（由 v1 轉換的紀錄為空字串：不比對）
    pub sha256: String,
    /// 簽署金鑰的指紋（`SHA256:…`）
    #[serde(default)]
    pub signer: String,
}

/// tar：這次要替換 unit 時的新舊雜湊（由 v1 轉換的紀錄為空字串：不比對）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitPlan {
    pub old_sha256: String,
    pub new_sha256: String,
}

/// 回滾的原因類別
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// 健康確認沒有通過
    Health,
    /// 替換無法完成而系統已被改過
    ReplaceFailed,
}

/// 還原的方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// 以 `rollback.pkg` 經套件管理員裝回
    Package,
    /// 換回 `pulse.prev`（tar 另還原 unit）
    Binary,
}

/// 回滾的意圖（還原之前寫入）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackIntent {
    pub reason: String,
    pub cause: Cause,
    pub via: Via,
}

/// 交易紀錄（`state.json`，schema v2）。欄位說明見 REMOTE.md 第 17.9.2 節
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema: String,
    pub checksum: String,
    pub upgrade_id: String,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub updated_at: String,
    pub step: Step,
    pub from_version: String,
    #[serde(default)]
    pub from_commit: String,
    pub target: UpgradeTarget,
    pub method: String,
    #[serde(default)]
    pub old_binary_sha256: String,
    #[serde(default)]
    pub artifact: Option<Verified>,
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default)]
    pub new_binary_sha256: String,
    #[serde(default)]
    pub rollback: Option<Verified>,
    #[serde(default)]
    pub unit: Option<UnitPlan>,
    #[serde(default)]
    pub pkg_db_before: Option<String>,
    #[serde(default)]
    pub pkg_db_after: Option<String>,
    #[serde(default)]
    pub binding_sha256: String,
    #[serde(default)]
    pub prev_saved: bool,
    #[serde(default)]
    pub unit_backed_up: bool,
    #[serde(default)]
    pub unit_replaced: bool,
    #[serde(default)]
    pub health_timeout_secs: u64,
    #[serde(default)]
    pub deadline_unix: i64,
    #[serde(default)]
    pub n_restarts_before: u64,
    #[serde(default)]
    pub restarted_unix: i64,
    /// 這個步驟（`replaced`／`rolling_back`）的 `systemctl restart` 已經送出。接續時還是 `false` 就再送
    /// 一次：輔助程式可能只是自己被終止（OOM、人工 `kill` 等）而主機沒有重新開機，此時服務仍是
    /// 替換前啟動的那個行程。期限不重新起算（REMOTE.md 第 17.9.3 節）
    #[serde(default)]
    pub service_restarted: bool,
    #[serde(default)]
    pub rollback_intent: Option<RollbackIntent>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub result: Option<UpgradeResult>,
    /// 由 v1 的狀態檔轉換而來（沒有雜湊可比對）
    #[serde(default)]
    pub legacy: bool,
}

impl Record {
    /// 新交易的第一份紀錄（`preparing`）
    pub fn new(upgrade_id: &str, from_version: &str, from_commit: &str, method: &str) -> Self {
        Self {
            schema: RECORD_SCHEMA.to_string(),
            checksum: String::new(),
            upgrade_id: upgrade_id.to_string(),
            seq: 0,
            updated_at: String::new(),
            step: Step::Preparing,
            from_version: from_version.to_string(),
            from_commit: from_commit.to_string(),
            target: UpgradeTarget {
                version: String::new(),
                commit: String::new(),
            },
            method: method.to_string(),
            old_binary_sha256: String::new(),
            artifact: None,
            manifest: None,
            new_binary_sha256: String::new(),
            rollback: None,
            unit: None,
            pkg_db_before: None,
            pkg_db_after: None,
            binding_sha256: String::new(),
            prev_saved: false,
            unit_backed_up: false,
            unit_replaced: false,
            health_timeout_secs: 0,
            deadline_unix: 0,
            n_restarts_before: 0,
            restarted_unix: 0,
            service_restarted: false,
            rollback_intent: None,
            notes: Vec::new(),
            result: None,
            legacy: false,
        }
    }

    /// 綁定欄位的 SHA-256（`prepared` 時算一次，寫進紀錄與指標；之後只比對字串，不重新計算——
    /// 接續的可能是另一個版本的輔助程式）
    pub fn compute_binding(&self) -> String {
        let v = |x: &Option<Verified>| match x {
            Some(v) => format!("{}|{}|{}", v.name, v.sha256, v.signer),
            None => "-".to_string(),
        };
        let unit = match &self.unit {
            Some(u) => format!("{}|{}", u.old_sha256, u.new_sha256),
            None => "-".to_string(),
        };
        let text = [
            "pulse.upgrade-binding/v1".to_string(),
            self.upgrade_id.clone(),
            self.from_version.clone(),
            self.from_commit.clone(),
            self.target.version.clone(),
            self.target.commit.clone(),
            self.method.clone(),
            self.old_binary_sha256.clone(),
            v(&self.artifact),
            self.manifest.clone().unwrap_or_else(|| "-".to_string()),
            self.new_binary_sha256.clone(),
            v(&self.rollback),
            unit,
            self.pkg_db_before
                .clone()
                .unwrap_or_else(|| "-".to_string()),
            self.health_timeout_secs.to_string(),
        ]
        .join("\n");
        files::hex(ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref())
    }

    /// 序列化並加上 checksum
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut r = self.clone();
        r.checksum = CHECKSUM_PLACEHOLDER.to_string();
        let text = serde_json::to_string_pretty(&r).map_err(|e| format!("無法產生狀態檔：{e}"))?;
        seal(&text).map(String::into_bytes)
    }

    /// 寫入 `dir/state.json`（耐久；`seq` 與 `updated_at` 由這裡更新）
    pub fn save(&mut self, dir: &Path) -> Result<(), String> {
        self.seq += 1;
        self.updated_at = crate::protocol::now_ts();
        let bytes = self.to_bytes()?;
        let path = dir.join(STATE_FILE);
        crate::remote::write_atomic_durable(&path, &bytes, 0o600, 0o700)
            .map_err(|e| format!("無法寫入 {}：{e}", path.display()))
    }
}

/// 把內容中的 checksum（必須是佔位值）換成整份內容的 SHA-256
fn seal(text: &str) -> Result<String, String> {
    let needle = format!("{CHECKSUM_KEY}{CHECKSUM_PLACEHOLDER}\"");
    let Some(at) = text.find(&needle) else {
        return Err("內容沒有 checksum 欄位".to_string());
    };
    let sum = files::hex(ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref());
    let start = at + CHECKSUM_KEY.len();
    Ok(format!(
        "{}{sum}{}",
        &text[..start],
        &text[start + CHECKSUM_PLACEHOLDER.len()..]
    ))
}

/// 檢查內容的 checksum（以檔案的原始位元組計算：不受欄位增減影響）
fn verify_seal(bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_string())?;
    // JSON 字串裡的引號一定被跳脫，所以第一個符合的位置是真正的鍵（不會落在某個字串值裡）
    let at = text
        .find(CHECKSUM_KEY)
        .ok_or_else(|| "no checksum".to_string())?;
    let start = at + CHECKSUM_KEY.len();
    let found = text
        .get(start..start + CHECKSUM_PLACEHOLDER.len())
        .filter(|s| s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| "malformed checksum".to_string())?;
    let zeroed = format!(
        "{}{CHECKSUM_PLACEHOLDER}{}",
        &text[..start],
        &text[start + CHECKSUM_PLACEHOLDER.len()..]
    );
    let sum = files::hex(ring::digest::digest(&ring::digest::SHA256, zeroed.as_bytes()).as_ref());
    if sum == found {
        Ok(())
    } else {
        Err("checksum mismatch".to_string())
    }
}

/// 測試用：內容被改寫後重新計算 checksum
#[cfg(test)]
pub fn reseal(text: &str) -> String {
    let at = text.find(CHECKSUM_KEY).expect("checksum") + CHECKSUM_KEY.len();
    let zeroed = format!(
        "{}{CHECKSUM_PLACEHOLDER}{}",
        &text[..at],
        &text[at + CHECKSUM_PLACEHOLDER.len()..]
    );
    seal(&zeroed).expect("seal")
}

/// v1 的步驟
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyStep {
    Verified,
    Replaced,
    RollingBack,
    Done,
}

/// v1 的狀態檔（0.9.0；0.10.0 開發期間另加了 `from_version` 之後的欄位，未曾發佈）
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LegacyState {
    pub step: LegacyStep,
    #[serde(default)]
    pub deadline_unix: i64,
    #[serde(default)]
    pub n_restarts_before: u64,
    #[serde(default)]
    pub restarted_unix: i64,
    #[serde(default)]
    pub health_timeout_secs: u64,
    #[serde(default)]
    pub unit_replaced: bool,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub rollback_reason: String,
    #[serde(default)]
    pub from_version: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub target: Option<UpgradeTarget>,
    #[serde(default)]
    pub rollback_artifact: Option<String>,
    #[serde(default)]
    pub result: Option<UpgradeResult>,
}

/// 讀取工作目錄的交易紀錄的結果
#[derive(Debug)]
pub enum Loaded {
    /// 沒有 `state.json`
    Missing,
    V2(Box<Record>),
    Legacy(Box<LegacyState>),
    /// 存在但讀不到、不是合法的內容、checksum 不符或 schema 不認識
    Unusable(String),
}

/// 讀取 `dir/state.json`
pub fn load_record(dir: &Path) -> Loaded {
    let path = dir.join(STATE_FILE);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Missing,
        Err(e) => return Loaded::Unusable(format!("{STATE_FILE} cannot be read: {e}")),
        Ok(_) => {}
    }
    let bytes = match files::read_untrusted_file(&path, MAX_RECORD_BYTES) {
        Ok(b) => b,
        Err(e) => return Loaded::Unusable(format!("{STATE_FILE} cannot be read: {e}")),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return Loaded::Unusable(format!("{STATE_FILE} is not valid: {e}")),
    };
    match value.get("schema").and_then(|s| s.as_str()) {
        Some(RECORD_SCHEMA) => {
            if let Err(e) = verify_seal(&bytes) {
                return Loaded::Unusable(format!("{STATE_FILE}: {e}"));
            }
            match serde_json::from_value::<Record>(value) {
                Ok(r) => Loaded::V2(Box::new(r)),
                Err(e) => Loaded::Unusable(format!("{STATE_FILE} is not valid: {e}")),
            }
        }
        Some(LEGACY_SCHEMA) => match serde_json::from_value::<LegacyState>(value) {
            Ok(s) => Loaded::Legacy(Box::new(s)),
            Err(e) => Loaded::Unusable(format!("{STATE_FILE} is not valid: {e}")),
        },
        other => Loaded::Unusable(format!("{STATE_FILE} has schema {other:?}")),
    }
}

/// 進行中交易的指標
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Active {
    pub schema: String,
    pub checksum: String,
    pub upgrade_id: String,
    pub binding_sha256: String,
    #[serde(default)]
    pub created_at: String,
}

/// 讀取指標。`Ok(None)`：沒有進行中的交易；`Err`：檔案存在但讀不到、內容不合法或 checksum 不符
pub fn read_active(work_root: &Path) -> Result<Option<Active>, String> {
    let path = work_root.join(ACTIVE_FILE);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{ACTIVE_FILE} cannot be read: {e}")),
        Ok(_) => {}
    }
    let bytes = files::read_untrusted_file(&path, MAX_RECORD_BYTES)
        .map_err(|e| format!("{ACTIVE_FILE} cannot be read: {e}"))?;
    let a: Active =
        serde_json::from_slice(&bytes).map_err(|e| format!("{ACTIVE_FILE} is not valid: {e}"))?;
    if a.schema != ACTIVE_SCHEMA || !kairos_proto::is_uuid_text(&a.upgrade_id) {
        return Err(format!(
            "{ACTIVE_FILE} has an unexpected schema or upgrade_id"
        ));
    }
    verify_seal(&bytes).map_err(|e| format!("{ACTIVE_FILE}: {e}"))?;
    Ok(Some(a))
}

/// 指標的內容（含 checksum）
pub fn active_bytes(upgrade_id: &str, binding_sha256: &str) -> Result<Vec<u8>, String> {
    let a = Active {
        schema: ACTIVE_SCHEMA.to_string(),
        checksum: CHECKSUM_PLACEHOLDER.to_string(),
        upgrade_id: upgrade_id.to_string(),
        binding_sha256: binding_sha256.to_string(),
        created_at: crate::protocol::now_ts(),
    };
    let text =
        serde_json::to_string_pretty(&a).map_err(|e| format!("無法產生 {ACTIVE_FILE}：{e}"))?;
    seal(&text).map(String::into_bytes)
}

/// 寫入指標（耐久）
pub fn write_active(
    work_root: &Path,
    upgrade_id: &str,
    binding_sha256: &str,
) -> Result<(), String> {
    let path = work_root.join(ACTIVE_FILE);
    let bytes = active_bytes(upgrade_id, binding_sha256)?;
    crate::remote::write_atomic_durable(&path, &bytes, 0o600, 0o700)
        .map_err(|e| format!("無法寫入 {}：{e}", path.display()))
}

/// 刪除指標（不存在時視為成功），之後 fsync 目錄
pub fn clear_active(work_root: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(work_root.join(ACTIVE_FILE)) {
        Ok(()) => fsync_dir(work_root),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// fsync 目錄（Unix；其他平台不做事）
pub fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// 一般檔案（不跟隨符號連結）的 SHA-256；不存在、不是一般檔案或讀不到時 `None`
pub fn sha256_regular(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = o.open(path).ok()?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Some(files::hex(ctx.finish().as_ref()))
}

/// 耐久地複製檔案：寫到同目錄的暫存檔（新建、不跟隨符號連結）→ fsync → 改名 → fsync 目錄
pub fn copy_durable(src: &Path, dst: &Path, mode: u32) -> std::io::Result<()> {
    let dir = dst
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut name = std::ffi::OsString::from(".");
    name.push(dst.file_name().unwrap_or_default());
    name.push(".tmp-copy");
    let tmp = dir.join(name);
    let r = (|| {
        let _ = std::fs::remove_file(&tmp);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(mode).custom_flags(libc::O_NOFOLLOW);
        }
        let mut out = o.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            out.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut input = std::fs::File::open(src)?;
        std::io::copy(&mut input, &mut out)?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&tmp, dst)?;
        fsync_dir(dir)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// 路徑檢查時可接受的擁有者：正式環境為 root；測試（`require_root = false`）為執行測試的帳號或 root
#[derive(Debug, Clone, Copy)]
pub struct Owner {
    #[cfg_attr(not(unix), allow(dead_code))]
    require_root: bool,
}

impl Owner {
    pub fn new(require_root: bool) -> Self {
        Self { require_root }
    }

    #[cfg(unix)]
    fn accepts(self, uid: u32) -> bool {
        // SAFETY: geteuid 沒有前置條件，也不會失敗
        uid == 0 || (!self.require_root && uid == unsafe { libc::geteuid() })
    }
}

/// 路徑檢查沒有通過的一項。輔助程式（root）把任何一項都當成不安全；Pulse 在 hello 之前的預檢
/// （[`precheck`]，服務帳號）則略過 `denied` 的——那只代表服務帳號看不到，不代表路徑不安全
#[cfg(unix)]
#[derive(Debug)]
struct Issue {
    /// `<路徑>: <原因>`（結果 `detail` 中 `unsafe_path: ` 之後的文字）
    msg: String,
    /// 因為權限不足（`EACCES`）而無法檢查
    denied: bool,
}

#[cfg(unix)]
impl Issue {
    fn unsafe_path(msg: String) -> Self {
        Self { msg, denied: false }
    }

    /// `lstat`／`realpath` 失敗：`<路徑>: cannot be <動作>: <錯誤>`
    fn io(p: &Path, action: &str, e: &std::io::Error) -> Self {
        Self {
            msg: format!("{}: cannot be {action}: {e}", p.display()),
            denied: e.kind() == std::io::ErrorKind::PermissionDenied,
        }
    }
}

/// 檢查一個上層目錄（或路徑中的符號連結）：擁有者、群組與其他人不可寫（sticky 的目錄除外）
#[cfg(unix)]
fn check_component(p: &Path, owner: Owner) -> Result<(), Issue> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(p).map_err(|e| Issue::io(p, "examined", &e))?;
    if !owner.accepts(meta.uid()) {
        return Err(Issue::unsafe_path(format!(
            "{}: owned by uid {}, not root",
            p.display(),
            meta.uid()
        )));
    }
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if !meta.file_type().is_dir() {
        return Err(Issue::unsafe_path(format!(
            "{}: not a directory",
            p.display()
        )));
    }
    if meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0 {
        return Err(Issue::unsafe_path(format!(
            "{}: writable by group or others (mode {:o})",
            p.display(),
            meta.mode() & 0o7777
        )));
    }
    Ok(())
}

/// 檢查 `dir` 與它的每一個上層目錄（路徑中有符號連結時連同解析後的實際路徑）
#[cfg(unix)]
fn check_chain(dir: &Path, owner: Owner) -> Result<(), Issue> {
    for p in dir.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        check_component(p, owner)?;
    }
    let real = std::fs::canonicalize(dir).map_err(|e| Issue::io(dir, "resolved", &e))?;
    if real != dir {
        for p in real.ancestors() {
            check_component(p, owner)?;
        }
    }
    Ok(())
}

/// [`inspect_private_dir`] 的結果
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateDir {
    /// 不存在（上層目錄已通過檢查）
    Missing,
    /// 符合：群組與其他人沒有任何權限
    Private,
    /// 群組或其他人只多了讀取／執行權限（目前的權限）
    Readable(u32),
}

/// root 專用目錄的檢查（不做任何變更；[`ensure_private_dir`] 與 [`precheck`] 共用）：上層目錄通過
/// [`check_chain`]；目錄本身不是符號連結、是目錄、擁有者 root、群組與其他人不可寫
#[cfg(unix)]
fn inspect_private_dir(dir: &Path, owner: Owner) -> Result<PrivateDir, Issue> {
    use std::os::unix::fs::MetadataExt;
    let parent = dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Issue::unsafe_path(format!("{}: no parent directory", dir.display())))?;
    check_chain(parent, owner)?;
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PrivateDir::Missing),
        Err(e) => return Err(Issue::io(dir, "examined", &e)),
    };
    if meta.file_type().is_symlink() {
        return Err(Issue::unsafe_path(format!(
            "{}: is a symbolic link",
            dir.display()
        )));
    }
    if !meta.file_type().is_dir() {
        return Err(Issue::unsafe_path(format!(
            "{}: not a directory",
            dir.display()
        )));
    }
    if !owner.accepts(meta.uid()) {
        return Err(Issue::unsafe_path(format!(
            "{}: owned by uid {}, not root",
            dir.display(),
            meta.uid()
        )));
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o022 != 0 {
        return Err(Issue::unsafe_path(format!(
            "{}: writable by group or others (mode {mode:o})",
            dir.display()
        )));
    }
    if mode & 0o077 != 0 {
        return Ok(PrivateDir::Readable(mode));
    }
    Ok(PrivateDir::Private)
}

/// root 專用的目錄（工作目錄的根目錄與每筆交易的工作目錄）：必須是目錄、不是符號連結、擁有者 root、
/// 群組與其他人沒有任何權限；既存的目錄同樣檢查。不存在時以 0700 建立（上層目錄必須存在並通過檢查）。
/// 群組或其他人只多了讀取／執行權限時改回 0700，回傳 `Ok(Some(說明))` 供呼叫端記錄
pub fn ensure_private_dir(dir: &Path, owner: Owner) -> Result<Option<String>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut state = inspect_private_dir(dir, owner).map_err(|i| i.msg)?;
        if state == PrivateDir::Missing {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("{}: cannot be created: {e}", dir.display()))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("{}: cannot set mode: {e}", dir.display()))?;
            if let Some(parent) = dir.parent() {
                let _ = fsync_dir(parent);
            }
            // 剛建立的目錄同樣檢查（建立之後可能已被換掉）
            state = inspect_private_dir(dir, owner).map_err(|i| i.msg)?;
        }
        match state {
            PrivateDir::Private => Ok(None),
            PrivateDir::Readable(mode) => {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| format!("{}: cannot set mode: {e}", dir.display()))?;
                Ok(Some(format!(
                    "{} 的權限是 {mode:o}，已改回 700",
                    dir.display()
                )))
            }
            PrivateDir::Missing => Err(format!(
                "{}: cannot be examined: it disappeared after being created",
                dir.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = owner;
        crate::remote::create_dir_all_mode(dir, 0o700)
            .map_err(|e| format!("{}: cannot be created: {e}", dir.display()))?;
        Ok(None)
    }
}

/// 安裝目錄與執行檔的檢查（不做任何變更；[`check_install`] 與 [`precheck`] 共用）
#[cfg(unix)]
fn inspect_install(install_dir: &Path, owner: Owner) -> Result<(), Issue> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(install_dir)
        .map_err(|e| Issue::io(install_dir, "examined", &e))?;
    if meta.file_type().is_symlink() {
        return Err(Issue::unsafe_path(format!(
            "{}: is a symbolic link",
            install_dir.display()
        )));
    }
    check_chain(install_dir, owner)?;
    let bin = install_dir.join("pulse");
    let meta = std::fs::symlink_metadata(&bin).map_err(|e| Issue::io(&bin, "examined", &e))?;
    if !meta.file_type().is_file() {
        return Err(Issue::unsafe_path(format!(
            "{}: not a regular file",
            bin.display()
        )));
    }
    if !owner.accepts(meta.uid()) {
        return Err(Issue::unsafe_path(format!(
            "{}: owned by uid {}, not root",
            bin.display(),
            meta.uid()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(Issue::unsafe_path(format!(
            "{}: writable by group or others (mode {:o})",
            bin.display(),
            meta.mode() & 0o7777
        )));
    }
    let prev = install_dir.join("pulse.prev");
    match std::fs::symlink_metadata(&prev) {
        Ok(m) if !m.file_type().is_file() => {
            return Err(Issue::unsafe_path(format!(
                "{}: not a regular file",
                prev.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Issue::io(&prev, "examined", &e)),
    }
    Ok(())
}

/// 安裝目錄與執行檔（第 17.9.7 節）
pub fn check_install(install_dir: &Path, owner: Owner) -> Result<(), String> {
    #[cfg(unix)]
    {
        inspect_install(install_dir, owner).map_err(|i| i.msg)
    }
    #[cfg(not(unix))]
    {
        let _ = (install_dir, owner);
        Ok(())
    }
}

/// Pulse（服務帳號，不是 root）在 hello 之前的預檢（REMOTE.md 第 2.1、17.9.7 節）：與輔助程式在
/// [`check_install`]、[`ensure_private_dir`]（工作目錄的根目錄）做的是**同一套檢查**（共用
/// [`inspect_install`]、[`inspect_private_dir`]），但不建立、不修改任何東西，讓不合規的主機在資格預覽
/// 就被標出來（`install.upgrade = "unsafe_path"`），而不是等推送失敗（`PLS-E8405`）。
///
/// 與輔助程式的差別：
/// - **看不到的不算不安全**：`lstat`／`realpath` 因為權限不足（`EACCES`）失敗時略過那一部分（安裝目錄、
///   工作目錄的根目錄各自獨立），不回報。服務帳號看不到不代表路徑有問題；輔助程式（root）仍會檢查。
/// - 工作目錄的根目錄不存在（輔助程式第一次執行時才建立）、或群組與其他人只多了讀取／執行權限（輔助程式
///   會改回 0700）都算通過。
/// - **不涵蓋**每筆交易的工作目錄（`/var/lib/pulse-upgrade/<upgrade_id>`；根目錄是 `0700 root`，服務帳號
///   進不去）與其中的檔案：仍由輔助程式在執行時檢查。
///
/// 預檢只是提早告知，不取代輔助程式的檢查：兩者之間路徑可能改變，輔助程式每次執行都重新檢查
pub fn precheck(install_dir: &Path, work_root: &Path, owner: Owner) -> Result<(), String> {
    #[cfg(unix)]
    {
        let parts = [
            inspect_install(install_dir, owner),
            inspect_private_dir(work_root, owner).map(|_| ()),
        ];
        for part in parts {
            match part {
                Err(issue) if !issue.denied => return Err(issue.msg),
                _ => {}
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (install_dir, work_root, owner);
        Ok(())
    }
}

/// 工作目錄根目錄下的一筆（名稱是 UUID 的子目錄）
#[derive(Debug)]
pub struct Entry {
    pub id: String,
    pub path: PathBuf,
    pub kind: Kind,
    /// 排序用的時間（`state.json` 的修改時間；沒有時為目錄的）
    pub modified: std::time::SystemTime,
}

#[derive(Debug)]
pub enum Kind {
    /// 沒有 `state.json`
    NoRecord,
    /// `state.json` 不可用
    Unusable(String),
    V2 {
        step: Step,
        /// `done` 時的結果狀態
        result: Option<String>,
    },
    Legacy {
        done: bool,
        result: Option<String>,
    },
}

impl Kind {
    /// 已結束的紀錄的結果狀態
    fn finished(&self) -> Option<&str> {
        match self {
            Kind::V2 {
                step: Step::Done,
                result,
            } => Some(result.as_deref().unwrap_or("")),
            Kind::Legacy { done: true, result } => Some(result.as_deref().unwrap_or("")),
            _ => None,
        }
    }

    /// 清理時不得刪除：未結束或無法判斷的紀錄
    fn protected(&self) -> bool {
        match self {
            Kind::Unusable(_) => true,
            Kind::V2 { step, .. } => step.unfinished(),
            Kind::Legacy { done, .. } => !done,
            Kind::NoRecord => false,
        }
    }
}

/// 列出工作目錄根目錄下每一筆交易的工作目錄（只看名稱是 UUID 的子目錄，不跟隨符號連結）
pub fn scan(work_root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(work_root) else {
        return out;
    };
    for e in entries.flatten() {
        let id = e.file_name().to_string_lossy().to_string();
        if !kairos_proto::is_uuid_text(&id) {
            continue;
        }
        let path = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        let kind = match load_record(&path) {
            Loaded::Missing => Kind::NoRecord,
            Loaded::Unusable(why) => Kind::Unusable(why),
            Loaded::V2(r) => Kind::V2 {
                step: r.step,
                result: r.result.map(|x| x.state),
            },
            Loaded::Legacy(s) => Kind::Legacy {
                done: s.step == LegacyStep::Done,
                result: s.result.map(|x| x.state),
            },
        };
        let modified = std::fs::symlink_metadata(path.join(STATE_FILE))
            .and_then(|m| m.modified())
            .or_else(|_| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        out.push(Entry {
            id,
            path,
            kind,
            modified,
        });
    }
    out
}

/// 工作目錄中「日誌與紀錄以外」的檔名（交易的痕跡；自己的暫存檔不算）
pub fn traces(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != LOG_FILE && n != STATE_FILE && !n.starts_with(".state.json.tmp-"))
        .collect();
    names.sort();
    names
}

/// 只留下紀錄與日誌（交易結束後：發佈檔、`new.bin`、回滾套件等不再需要）
pub fn strip(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n == LOG_FILE || n == STATE_FILE {
            continue;
        }
        let p = e.path();
        let is_dir = std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_dir());
        let _ = if is_dir {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
    }
}

/// 只留下日誌（重新開始一筆還沒動手的交易）
pub fn reset(dir: &Path) {
    strip(dir);
    let _ = std::fs::remove_file(dir.join(STATE_FILE));
}

/// 清理工作目錄（第 17.9.8 節）：保留最近 1 筆已結束的紀錄與最近 [`KEEP_ROLLBACK_FAILED`] 筆
/// `rollback_failed`（只有最近的一筆保留全部檔案），沒有紀錄或還沒動手就中斷的殘留目錄刪除。
/// 不動：`keep` 列出的工作目錄、未結束或無法判斷的紀錄、名稱不是 UUID 的項目。`current`：交易結束時
/// 為這筆交易（`keep` 的那一筆）的結果狀態——它就是「最近 1 筆已結束的紀錄」，是 `rollback_failed` 時也
/// 算進保留數；開始新的交易之前為 `None`。回傳刪除的 `upgrade_id`
pub fn prune(work_root: &Path, keep: &[&str], current: Option<&str>) -> Vec<String> {
    let latest_is_kept = current.is_some();
    let mut removed = Vec::new();
    let mut finished: Vec<Entry> = Vec::new();
    for e in scan(work_root) {
        if keep.contains(&e.id.as_str()) || e.kind.protected() {
            continue;
        }
        if e.kind.finished().is_some() {
            finished.push(e);
        } else if std::fs::remove_dir_all(&e.path).is_ok() {
            // 沒有紀錄，或紀錄停在 preparing／prepared（沒有指標：還沒動手）
            removed.push(e.id);
        }
    }
    finished.sort_by_key(|e| std::cmp::Reverse(e.modified));
    let mut failed_kept =
        usize::from(current == Some(kairos_proto::mgmt::UPGRADE_STATE_ROLLBACK_FAILED));
    for (i, e) in finished.into_iter().enumerate() {
        let failed = e.kind.finished() == Some(kairos_proto::mgmt::UPGRADE_STATE_ROLLBACK_FAILED);
        if failed && failed_kept < KEEP_ROLLBACK_FAILED {
            failed_kept += 1;
            if failed_kept > 1 {
                strip(&e.path);
            }
        } else if i == 0 && !latest_is_kept {
            // 最近一筆已結束的紀錄：只留紀錄與日誌（大檔通常在結束時已刪除；在記下 done 之後、清理之前
            // 中斷而且沒有再被觸發的交易會留著）
            strip(&e.path);
        } else if std::fs::remove_dir_all(&e.path).is_ok() {
            removed.push(e.id);
        }
    }
    if !removed.is_empty() {
        let _ = fsync_dir(work_root);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_detects_any_change_and_survives_unknown_fields() {
        let mut r = Record::new(
            "9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70",
            "0.9.0",
            "1a2b3c4",
            "tar",
        );
        r.notes
            .push("a note with \"checksum\": \"deadbeef\" inside".to_string());
        let bytes = r.to_bytes().unwrap();
        verify_seal(&bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // 任何位元組被改過
        let tampered = text.replace("\"tar\"", "\"deb\"");
        assert_eq!(
            verify_seal(tampered.as_bytes()).unwrap_err(),
            "checksum mismatch"
        );
        // 另一個版本多寫的欄位：checksum 以原始位元組計算，仍然相符，解析時忽略
        let extra = reseal(&text.replacen(
            "\"upgrade_id\"",
            "\"added_by_a_newer_helper\": 1,\n  \"upgrade_id\"",
            1,
        ));
        verify_seal(extra.as_bytes()).unwrap();
        let back: Record = serde_json::from_str(&extra).unwrap();
        assert_eq!(back.upgrade_id, r.upgrade_id);
        // 截斷
        assert!(verify_seal(&text.as_bytes()[..text.len() / 2]).is_err());
        assert!(verify_seal(b"{}").is_err());
    }

    #[test]
    fn binding_covers_the_fields_that_decide_what_gets_installed() {
        let mut r = Record::new(
            "9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70",
            "0.9.0",
            "1a2b3c4",
            "deb",
        );
        r.old_binary_sha256 = "aa".into();
        let base = r.compute_binding();
        let mut changed = Vec::new();
        let mut x = r.clone();
        x.from_version = "0.8.0".into();
        changed.push(x);
        let mut x = r.clone();
        x.target.version = "0.9.9".into();
        changed.push(x);
        let mut x = r.clone();
        x.method = "tar".into();
        changed.push(x);
        let mut x = r.clone();
        x.old_binary_sha256 = "bb".into();
        changed.push(x);
        let mut x = r.clone();
        x.artifact = Some(Verified {
            name: "a".into(),
            sha256: "cc".into(),
            signer: String::new(),
        });
        changed.push(x);
        let mut x = r.clone();
        x.rollback = Some(Verified {
            name: "r".into(),
            sha256: "dd".into(),
            signer: String::new(),
        });
        changed.push(x);
        let mut x = r.clone();
        x.pkg_db_before = Some("0.9.0-1 ii".into());
        changed.push(x);
        for c in changed {
            assert_ne!(c.compute_binding(), base, "{c:?}");
        }
        // 步驟與進度不屬於綁定
        r.step = Step::Replaced;
        r.prev_saved = true;
        assert_eq!(r.compute_binding(), base);
    }
}
