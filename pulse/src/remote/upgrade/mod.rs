//! 遠端升級（P2，Linux；`crates/kairos-proto/REMOTE.md` 第 4 節、PROTOCOL.md「遠端管理」）。
//!
//! Pulse（服務帳號）只負責：評估提議 → 分段下載（`upgrade_chunk_get`，最多 2 段在途、斷線續傳、
//! `rate_limited` 依 `retry_after_ms` 等待）→ 第一次驗證（本機信任錨、`SHA256SUMS`、檔名與平台、版本政策）→
//! 暫存 → 在 `apply_window` 內寫 `request.json` 交給 root 的 `pulse-upgrade.service` → 替換後的 Pulse
//! 讀 `result.json` 回報。替換、重新啟動、健康確認與回滾在輔助程式（[`helper`]），它重新驗證所有檔案。
//!
//! 狀態持久化在 `<狀態目錄>/upgrade/state.json`（[`StateFile`]）：下載中 → 續傳；已暫存 → 等待時段；
//! 替換中 → 讀 `result.json`。`upgrade_status` 先放進 `outbox`、收到 `ack` 才移除（at-least-once；
//! Kairos 以 (`upgrade_id`, `state`) 去重）。同一時間只處理一個升級；集中設定的確認期內不交付。
//!
//! 與規格的差異（MVP）：外部 URL（`source: "url"`）在 0.10.0，0.9.0 一律 `declined`（`PLS-W8411`，
//! `detail: url_not_allowed`）。deb／rpm 的回滾檔只在本機有它所屬發佈的「驗證過的」`SHA256SUMS` 時才下載與
//! 使用（`<狀態目錄>/upgrade/releases/<版本>/`，每次驗證通過的發佈都會保存）；否則輔助程式改以替換前保留的
//! 執行檔回復（`detail` 註明套件資料庫仍為新版），不以 root 安裝未經信任錨驗證的套件。

pub mod files;
pub mod helper;
#[cfg(test)]
pub mod testsign;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use kairos_proto::mgmt::{
    self, ACTION_DOWNGRADE, ArtifactRef, InstallInfo, MAX_CHUNK_BYTES, MAX_CHUNKS_IN_FLIGHT,
    MAX_DOWNLOAD_HASH_MISMATCHES, Pending, SOURCE_KAIROS, UPGRADE_STATE_ACCEPTED,
    UPGRADE_STATE_APPLYING, UPGRADE_STATE_DECLINED, UPGRADE_STATE_EXPIRED, UPGRADE_STATE_FAILED,
    UPGRADE_STATE_ROLLBACK_FAILED, UPGRADE_STATE_ROLLED_BACK, UPGRADE_STATE_STAGED,
    UPGRADE_STATE_SUCCEEDED, UpgradeChunk, UpgradeOffer, UpgradeProgress, UpgradeStatus,
};
use kairos_proto::release::{
    self, AllowedSigners, Arch, ArtifactExpectation, Format, Os, PolicyDecline, Product,
    ReleaseBundle, ReleaseError, TrustPolicy, Version,
};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::codes;
use crate::log_throttle::LogThrottle;
use crate::remote::install::{self, Detection};

pub use files::{Request, UpgradeResult};

/// `state.json` 的 `schema`
pub const STATE_SCHEMA: &str = "pulse.upgrade-state/v1";
/// 保留的歷史筆數
const MAX_HISTORY: usize = 10;
/// `outbox` 的上限（超過時丟掉最舊的非終止狀態）
const MAX_OUTBOX: usize = 32;
/// 同一則 `upgrade_status` 被拒（`bad_request`）或結果不明幾次後放棄
const MAX_STATUS_ATTEMPTS: u32 = 3;
/// `storage_unavailable` 後重送 `upgrade_status` 的延遲
const STATUS_RETRY: Duration = Duration::from_secs(30);
/// 送出 `applying` 後最多等這麼久的 `ack` 才寫 `request.json`
const HANDOFF_ACK_WAIT: Duration = Duration::from_secs(15);
/// 替換中輪詢 `result.json` 的間隔
const RESULT_POLL: Duration = Duration::from_secs(5);
/// 等待 `apply_window.not_before` 時，最多隔這麼久再檢查一次（時鐘調整）
const WINDOW_RECHECK: Duration = Duration::from_secs(60);
/// 交付前向 Kairos 再確認提議（`upgrade_get`）被拒（推送已暫停或取消）後，同一條連線上多久再確認一次
const RECONFIRM_RETRY: Duration = Duration::from_secs(600);
/// `upgrade.download_failed` 的節流視窗（log-ux.md §4.6：10 分鐘）
const DOWNLOAD_LOG_WINDOW: Duration = Duration::from_secs(600);
/// 驗證過的發佈的簽章資料（回滾檔的信任來源）
const RELEASES_DIR: &str = "releases";

/// 遠端升級的設定與環境（啟動時決定；測試時以暫存目錄模擬）
#[derive(Debug, Clone)]
pub struct UpgradeEnv {
    /// Pulse 的狀態目錄（升級目錄為其下的 `upgrade/`）
    pub state_dir: PathBuf,
    pub detection: Detection,
    /// 目前的版本（`CARGO_PKG_VERSION`）
    pub current_version: String,
    /// 目前的 CPU 架構（`std::env::consts::ARCH`）
    pub arch: String,
    pub extra_principals: Vec<String>,
    /// 本機的 `upgrade.allow_downgrade`
    pub allow_downgrade: bool,
    /// 本機的 `upgrade.health_timeout_secs`
    pub health_timeout_secs: u32,
    /// 信任錨必須由 root 擁有（測試時 `false`）
    pub require_root_owner: bool,
    /// 目錄所在檔案系統的可用空間（bytes）；取不到時 `None`（不檢查）
    pub free_space: fn(&Path) -> Option<u64>,
}

/// 檔案系統的可用空間（Unix：`statvfs` 的 `f_bavail × f_frsize`）
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let mut p = path.to_path_buf();
        while !p.exists() {
            p = p.parent()?.to_path_buf();
        }
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
        // SAFETY: statvfs 只寫入我們提供的結構，路徑為合法的 C 字串
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: 同上
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

// ---------------------------------------------------------------------------
// 持久化的狀態
// ---------------------------------------------------------------------------

/// 目前升級的階段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 已接受、下載中
    Downloading,
    /// 下載並驗證完成，等待 `apply_window`
    Staged,
    /// 已送出 `applying`，等待 `ack` 後寫 `request.json`
    Handoff,
    /// 已寫 `request.json`，等待輔助程式的 `result.json`
    Applying,
}

/// 要下載的一個檔案
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileItem {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub done: bool,
}

/// 進行中的升級
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Current {
    pub upgrade_id: String,
    pub offer: UpgradeOffer,
    pub phase: Phase,
    pub from_version: String,
    pub to_version: String,
    /// `tar`、`deb`、`rpm`
    pub method: String,
    pub files: Vec<FileItem>,
    /// 下載完成後 SHA-256 不符的次數
    #[serde(default)]
    pub mismatches: u32,
    /// 已回報的進度（每 25% 一格）
    #[serde(default)]
    pub quarter: u8,
    /// 實際使用的健康確認期限
    pub health_timeout_secs: u32,
    /// 回滾檔所屬的發佈在本機有驗證過的紀錄
    #[serde(default)]
    pub rollback_trusted: bool,
    pub accepted_at: String,
    #[serde(default)]
    pub applying_at: Option<String>,
}

/// 結束的升級（`pulse upgrade status`、`pulse status`；也用來判斷 `pending.upgrade_id` 是否已處理過）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finished {
    pub upgrade_id: String,
    pub state: String,
    pub code: Option<String>,
    #[serde(default)]
    pub detail: String,
    pub from_version: String,
    pub to_version: String,
    pub at: String,
}

/// 待送（或已送、尚未收到 `ack`）的 `upgrade_status`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    pub upgrade_id: String,
    pub state: String,
    pub from_version: String,
    pub to_version: String,
    pub code: Option<String>,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub progress: Option<UpgradeProgress>,
    #[serde(default)]
    pub attempts: u32,
}

impl StatusRecord {
    /// 組成訊息
    pub fn message(&self, seq: u64) -> UpgradeStatus {
        UpgradeStatus {
            seq,
            ts: crate::protocol::now_ts(),
            upgrade_id: self.upgrade_id.clone(),
            state: self.state.clone(),
            from_version: self.from_version.clone(),
            to_version: self.to_version.clone(),
            code: self.code.clone(),
            detail: self.detail.clone(),
            progress: self.progress,
        }
    }

    fn key(&self) -> (String, String, Option<UpgradeProgress>) {
        (self.upgrade_id.clone(), self.state.clone(), self.progress)
    }
}

/// `<狀態目錄>/upgrade/state.json`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StateFile {
    pub schema: String,
    pub current: Option<Current>,
    pub history: Vec<Finished>,
    pub outbox: Vec<StatusRecord>,
}

impl StateFile {
    fn load(path: &Path) -> Result<Self, String> {
        match crate::remote::read_small(path, 1024 * 1024) {
            Ok(b) => {
                let s: StateFile = serde_json::from_slice(&b).map_err(|e| e.to_string())?;
                if s.schema != STATE_SCHEMA {
                    return Err(format!("schema 不是 {STATE_SCHEMA}"));
                }
                Ok(s)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// 每條連線的狀態
// ---------------------------------------------------------------------------

/// 送給 Kairos 的請求（`conn` 配上 `seq` 後送出）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    Get(String),
    Chunk {
        upgrade_id: String,
        filename: String,
        offset: u64,
        length: u32,
    },
    Status(StatusRecord),
}

/// 一條連線上的升級狀態（斷線即丟棄）
#[derive(Debug, Default)]
pub struct ConnState {
    negotiated: bool,
    /// 要送的 `upgrade_get`
    get_queue: Option<String>,
    get_in_flight: Option<String>,
    /// 在途的段落請求：(檔名, offset, length)
    chunks: Vec<(String, u64, u32)>,
    /// 在途的回應與預期不符：等在途的都回來後從檔案長度重新請求
    resync: bool,
    /// `rate_limited` 後等到這個時間
    retry_until: Option<Instant>,
    /// 這條連線已送出（等待 `ack`）的狀態
    status_in_flight: Vec<(String, String, Option<UpgradeProgress>)>,
    /// `storage_unavailable` 後到這個時間才重送狀態
    status_retry_until: Option<Instant>,
    /// 送出 `applying` 的時間
    handoff_since: Option<Instant>,
    next_result_poll: Option<Instant>,
    /// 這條連線收到 `upgrade_unavailable` 的 `upgrade_get`（不再重複要求）
    unavailable: Vec<String>,
    /// 交付前已在這條連線上向 Kairos 再確認過提議（推送仍在進行）
    reconfirmed: bool,
    /// 再確認被拒（推送已暫停或取消）：到這個時間前不再確認、不交付
    reconfirm_after: Option<Instant>,
}

// ---------------------------------------------------------------------------
// 代理
// ---------------------------------------------------------------------------

struct Inner {
    env: UpgradeEnv,
    file: StateFile,
    state_error: Option<String>,
    download_log: LogThrottle,
}

/// 遠端升級的狀態（跨連線保留）
pub struct UpgradeAgent {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for UpgradeAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeAgent").finish_non_exhaustive()
    }
}

/// `pulse status`／`pulse upgrade status` 用的摘要
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeView {
    pub method: String,
    /// `install.upgrade`
    pub readiness: String,
    pub reason: Option<String>,
    /// 進行中的升級：`downloading`、`staged`、`handoff`、`applying`
    pub phase: Option<String>,
    pub upgrade_id: Option<String>,
    pub to_version: Option<String>,
    pub bytes: Option<u64>,
    pub total: Option<u64>,
    pub not_before: Option<String>,
    pub last: Option<Finished>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn jitter(d: Duration) -> Duration {
    // ±20%（與重連退避相同的亂數來源）
    let spread = d.mul_f64(0.4);
    d.mul_f64(0.8) + crate::backoff::random_spread(spread)
}

impl UpgradeAgent {
    /// 啟動時建立：讀 `state.json`；替換中而 `result.json` 已在時立即處理（重新啟動後接續回報）
    pub fn new(env: UpgradeEnv) -> Arc<Self> {
        let path = Self::state_path_of(&env);
        let (file, state_error) = match StateFile::load(&path) {
            Ok(f) => (f, None),
            Err(e) => {
                log::warn!(
                    "{}遠端升級的狀態檔 {} 無法使用（{e}），重新開始",
                    codes::tag(codes::UPGRADE_FAILED, None),
                    path.display()
                );
                (StateFile::default(), Some(e))
            }
        };
        if env.detection.upgrade == mgmt::INSTALL_UPGRADE_NO_TRUST_ANCHOR {
            log::error!(
                "{}遠端升級已停用：信任錨無法使用（{}）；以 root 擁有、0644 放置官方公鑰後重新啟動 Pulse  path={} reason={}",
                codes::tag(
                    codes::UPGRADE_TRUST_ANCHOR_UNAVAILABLE,
                    Some(codes::E_UPGRADE_TRUST_ANCHOR)
                ),
                env.detection.reason.as_deref().unwrap_or(""),
                env.detection.trust_anchor.display(),
                env.detection.reason.as_deref().unwrap_or("")
            );
        } else if env.detection.upgrade == mgmt::INSTALL_UPGRADE_DISABLED {
            log::info!(
                "{}遠端升級已在本機停用（upgrade.enabled = false）",
                codes::tag(codes::UPGRADE_DECLINED, Some(codes::I_UPGRADE_DISABLED))
            );
        } else if env.detection.upgrade != mgmt::INSTALL_UPGRADE_READY {
            log::info!(
                "遠端升級不可用（install.upgrade = {}）：{}",
                env.detection.upgrade,
                env.detection.reason.as_deref().unwrap_or("")
            );
        }
        let agent = Arc::new(Self {
            inner: Mutex::new(Inner {
                env,
                file,
                state_error,
                download_log: LogThrottle::new(DOWNLOAD_LOG_WINDOW),
            }),
        });
        {
            let mut inner = agent.lock();
            Self::poll_result(&mut inner);
        }
        agent
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn state_path_of(env: &UpgradeEnv) -> PathBuf {
        files::upgrade_dir(&env.state_dir).join(files::STATE_FILE)
    }

    fn upgrade_dir(inner: &Inner) -> PathBuf {
        files::upgrade_dir(&inner.env.state_dir)
    }

    fn staging_dir(inner: &Inner, id: &str) -> PathBuf {
        Self::upgrade_dir(inner).join(id)
    }

    fn persist(inner: &mut Inner) {
        let path = Self::state_path_of(&inner.env);
        let mut f = inner.file.clone();
        f.schema = STATE_SCHEMA.to_string();
        let result = serde_json::to_vec_pretty(&f)
            .map_err(std::io::Error::other)
            .and_then(|b| {
                crate::remote::write_atomic(
                    &path,
                    &b,
                    crate::remote::FILE_MODE,
                    crate::remote::DIR_MODE,
                )
            });
        match result {
            Ok(()) => inner.state_error = None,
            Err(e) => {
                let e = e.to_string();
                if inner.state_error.as_deref() != Some(e.as_str()) {
                    log::warn!(
                        "無法寫入遠端升級的狀態檔 {}：{e}（重新啟動後可能重新下載或重送狀態）",
                        path.display()
                    );
                }
                inner.state_error = Some(e);
            }
        }
    }

    /// hello 的 `install`（`method`、`upgrade`；`config` 由集中設定補上）
    pub fn install_info(&self) -> InstallInfo {
        let inner = self.lock();
        InstallInfo {
            method: Some(inner.env.detection.method.to_string()),
            upgrade: Some(inner.env.detection.upgrade.to_string()),
            config: None,
        }
    }

    fn known(inner: &Inner, cs: &ConnState, id: &str) -> bool {
        inner
            .file
            .current
            .as_ref()
            .is_some_and(|c| c.upgrade_id.eq_ignore_ascii_case(id))
            || inner
                .file
                .history
                .iter()
                .any(|h| h.upgrade_id.eq_ignore_ascii_case(id))
            || cs.unavailable.iter().any(|u| u.eq_ignore_ascii_case(id))
    }

    /// 新連線收到 `hello_ack` 之後
    pub fn on_registered(&self, negotiated: bool, pending: Option<&Pending>) -> ConnState {
        let mut cs = ConnState {
            negotiated,
            ..Default::default()
        };
        if !negotiated {
            return cs;
        }
        let mut inner = self.lock();
        Self::poll_result(&mut inner);
        if let Some(c) = &inner.file.current {
            match c.phase {
                Phase::Downloading => {
                    let (done, total) = Self::progress(&inner, c);
                    if done > 0 {
                        log::info!("繼續下載 Pulse {}（{done}／{total} bytes）", c.to_version);
                    }
                }
                Phase::Handoff => cs.handoff_since = Some(Instant::now()),
                Phase::Applying => cs.next_result_poll = Some(Instant::now() + RESULT_POLL),
                Phase::Staged => {}
            }
        }
        drop(inner);
        if let Some(p) = pending {
            self.on_pending(&mut cs, p);
        }
        cs
    }

    /// `pending.upgrade_id`：不認得的升級要送 `upgrade_get`
    pub fn on_pending(&self, cs: &mut ConnState, p: &Pending) {
        if !cs.negotiated {
            return;
        }
        let inner = self.lock();
        if let Some(id) = p.upgrade_id.as_deref()
            && !Self::known(&inner, cs, id)
            && cs.get_in_flight.is_none()
            && !inner
                .file
                .current
                .as_ref()
                .is_some_and(|c| matches!(c.phase, Phase::Handoff | Phase::Applying))
        {
            cs.get_queue = Some(id.to_string());
        }
    }

    fn progress(inner: &Inner, c: &Current) -> (u64, u64) {
        let dir = Self::staging_dir(inner, &c.upgrade_id);
        let total: u64 = c.files.iter().map(|f| f.size).sum();
        let done: u64 = c
            .files
            .iter()
            .map(|f| {
                if f.done {
                    f.size
                } else {
                    std::fs::metadata(dir.join(format!("{}.part", f.name)))
                        .map_or(0, |m| m.len().min(f.size))
                }
            })
            .sum();
        (done, total)
    }

    /// 下一則要送的請求（`None`＝目前沒有）
    pub fn poll(&self, cs: &mut ConnState, now: Instant) -> Option<Out> {
        if !cs.negotiated {
            return None;
        }
        let inner = self.lock();
        // 1. 狀態（outbox，依序；同一則在這條連線上只送一次）
        if cs.status_retry_until.is_none_or(|t| now >= t) {
            cs.status_retry_until = None;
            if let Some(r) = inner
                .file
                .outbox
                .iter()
                .find(|r| !cs.status_in_flight.contains(&r.key()))
                .cloned()
            {
                cs.status_in_flight.push(r.key());
                return Some(Out::Status(r));
            }
        }
        // 2. upgrade_get
        if cs.get_in_flight.is_none()
            && let Some(id) = cs.get_queue.take()
        {
            cs.get_in_flight = Some(id.clone());
            return Some(Out::Get(id));
        }
        // 3. 段落請求
        if cs.retry_until.is_some_and(|t| now < t) || cs.chunks.len() >= MAX_CHUNKS_IN_FLIGHT {
            return None;
        }
        cs.retry_until = None;
        if cs.resync {
            if !cs.chunks.is_empty() {
                return None;
            }
            cs.resync = false;
        }
        let c = inner.file.current.as_ref()?;
        if c.phase != Phase::Downloading {
            return None;
        }
        let id = c.upgrade_id.clone();
        let dir = Self::staging_dir(&inner, &id);
        let file = c.files.iter().find(|f| !f.done)?.clone();
        let on_disk =
            std::fs::metadata(dir.join(format!("{}.part", file.name))).map_or(0, |m| m.len());
        let next = cs
            .chunks
            .iter()
            .filter(|(n, ..)| *n == file.name)
            .map(|(_, o, l)| o + u64::from(*l))
            .max()
            .unwrap_or(on_disk);
        if next >= file.size {
            return None;
        }
        let length = u32::try_from((file.size - next).min(u64::from(MAX_CHUNK_BYTES)))
            .unwrap_or(MAX_CHUNK_BYTES);
        cs.chunks.push((file.name.clone(), next, length));
        drop(inner);
        Some(Out::Chunk {
            upgrade_id: id,
            filename: file.name,
            offset: next,
            length,
        })
    }

    /// 下一次需要 [`UpgradeAgent::poll`]／[`UpgradeAgent::tick`] 的時間
    pub fn next_wake(&self, cs: &ConnState, now: Instant) -> Option<Instant> {
        if !cs.negotiated {
            return None;
        }
        let inner = self.lock();
        let mut t: Option<Instant> = None;
        let mut at = |x: Instant| t = Some(t.map_or(x, |y| y.min(x)));
        let has_status = inner
            .file
            .outbox
            .iter()
            .any(|r| !cs.status_in_flight.contains(&r.key()));
        if has_status {
            at(cs.status_retry_until.unwrap_or(now));
        }
        if cs.get_queue.is_some() && cs.get_in_flight.is_none() {
            at(now);
        }
        if let Some(c) = &inner.file.current {
            match c.phase {
                Phase::Downloading => {
                    if cs.chunks.len() < MAX_CHUNKS_IN_FLIGHT
                        && !(cs.resync && !cs.chunks.is_empty())
                    {
                        at(cs.retry_until.unwrap_or(now));
                    }
                }
                Phase::Staged if c.offer.apply_window.contains(now_ms()) && !cs.reconfirmed => {
                    // 時段內：再確認被拒後到 reconfirm_after 再試；在途時等回應
                    if cs.get_in_flight.is_none() {
                        at(cs.reconfirm_after.unwrap_or(now).max(now));
                    }
                }
                Phase::Staged => {
                    let w = &c.offer.apply_window;
                    let ms = now_ms();
                    let target = kairos_proto::enroll::parse_rfc3339_ms(&w.not_before)
                        .ok()
                        .filter(|&nb| nb > ms)
                        .map_or(Duration::ZERO, |nb| {
                            Duration::from_millis(u64::try_from(nb - ms).unwrap_or(0))
                        });
                    at(now + target.min(WINDOW_RECHECK));
                }
                Phase::Handoff => {
                    at(cs.handoff_since.map_or(now, |s| s + HANDOFF_ACK_WAIT));
                }
                Phase::Applying => at(cs.next_result_poll.unwrap_or(now)),
            }
        }
        t
    }

    /// 定時的檢查：已暫存 → 進入時段時交付（集中設定的確認期內不交付）或超過時段時放棄；交付中 → 收到
    /// `applying` 的 `ack`（或逾時）後寫 `request.json`；替換中 → 讀 `result.json`
    pub fn tick(&self, cs: &mut ConnState, now: Instant, config_confirming: bool) {
        if !cs.negotiated {
            return;
        }
        let mut inner = self.lock();
        let Some(c) = inner.file.current.clone() else {
            return;
        };
        match c.phase {
            Phase::Staged => {
                let ms = now_ms();
                if c.offer.apply_window.has_expired(ms) {
                    log::info!(
                        "Pulse {} 的升級時段已過（not_after {}），放棄這次提議，等 Kairos 下一個時段再提議",
                        c.to_version,
                        c.offer.apply_window.not_after
                    );
                    Self::finish(&mut inner, UPGRADE_STATE_EXPIRED, None, String::new(), true);
                } else if c.offer.apply_window.contains(ms) && !config_confirming {
                    if cs.reconfirmed {
                        Self::start_handoff(&mut inner);
                        cs.handoff_since = Some(now);
                    } else if cs.reconfirm_after.is_none_or(|t| now >= t)
                        && cs.get_in_flight.is_none()
                    {
                        // 交付前向 Kairos 再確認：暫存之後推送可能已暫停或取消（upgrade_unavailable）
                        cs.reconfirm_after = Some(now + RECONFIRM_RETRY);
                        cs.get_queue = Some(c.upgrade_id.clone());
                    }
                }
            }
            Phase::Handoff => {
                if cs
                    .handoff_since
                    .is_some_and(|s| now >= s + HANDOFF_ACK_WAIT)
                {
                    Self::write_request(&mut inner);
                    cs.next_result_poll = Some(now + RESULT_POLL);
                }
            }
            Phase::Applying => {
                if cs.next_result_poll.is_none_or(|t| now >= t) {
                    Self::poll_result(&mut inner);
                    cs.next_result_poll = Some(now + RESULT_POLL);
                }
            }
            Phase::Downloading => {}
        }
    }

    fn push_status(inner: &mut Inner, r: StatusRecord) {
        if inner.file.outbox.len() >= MAX_OUTBOX
            && let Some(i) = inner
                .file
                .outbox
                .iter()
                .position(|x| !mgmt::is_terminal_upgrade_state(&x.state))
        {
            inner.file.outbox.remove(i);
        }
        inner.file.outbox.push(r);
    }

    fn status_for(c: &Current, state: &str, code: Option<&str>, detail: String) -> StatusRecord {
        StatusRecord {
            upgrade_id: c.upgrade_id.clone(),
            state: state.to_string(),
            from_version: c.from_version.clone(),
            to_version: c.to_version.clone(),
            code: code.map(str::to_string),
            detail: truncate_detail(&detail),
            progress: None,
            attempts: 0,
        }
    }

    /// 結束目前的升級（終止狀態）：放進歷史與 outbox；`cleanup` 時刪除暫存
    fn finish(inner: &mut Inner, state: &str, code: Option<&str>, detail: String, cleanup: bool) {
        let Some(c) = inner.file.current.take() else {
            return;
        };
        let r = Self::status_for(&c, state, code, detail);
        inner.file.history.push(Finished {
            upgrade_id: c.upgrade_id.clone(),
            state: state.to_string(),
            code: r.code.clone(),
            detail: r.detail.clone(),
            from_version: c.from_version.clone(),
            to_version: c.to_version.clone(),
            at: crate::protocol::now_ts(),
        });
        if inner.file.history.len() > MAX_HISTORY {
            let n = inner.file.history.len() - MAX_HISTORY;
            inner.file.history.drain(..n);
        }
        Self::push_status(inner, r);
        if cleanup {
            let _ = std::fs::remove_dir_all(Self::staging_dir(inner, &c.upgrade_id));
        }
        Self::persist(inner);
    }

    /// 不經過「目前的升級」直接回報終止狀態（評估階段就拒絕的提議）
    fn report_terminal(
        inner: &mut Inner,
        offer: &UpgradeOffer,
        state: &str,
        code: Option<&str>,
        detail: String,
    ) {
        let r = StatusRecord {
            upgrade_id: offer.upgrade_id.to_ascii_lowercase(),
            state: state.to_string(),
            from_version: inner.env.current_version.clone(),
            to_version: offer.target.version.clone(),
            code: code.map(str::to_string),
            detail: truncate_detail(&detail),
            progress: None,
            attempts: 0,
        };
        inner.file.history.push(Finished {
            upgrade_id: r.upgrade_id.clone(),
            state: r.state.clone(),
            code: r.code.clone(),
            detail: r.detail.clone(),
            from_version: r.from_version.clone(),
            to_version: r.to_version.clone(),
            at: crate::protocol::now_ts(),
        });
        if inner.file.history.len() > MAX_HISTORY {
            let n = inner.file.history.len() - MAX_HISTORY;
            inner.file.history.drain(..n);
        }
        Self::push_status(inner, r);
        Self::persist(inner);
    }

    fn decline(inner: &mut Inner, offer: &UpgradeOffer, code: Option<&str>, detail: &str) {
        let (level, what) = if code == Some(codes::I_UPGRADE_DISABLED) {
            (log::Level::Info, "本機停用遠端升級")
        } else {
            (log::Level::Warn, "不接受")
        };
        log::log!(
            level,
            "{}{what} Pulse {} 的升級提議（{detail}）  upgrade_id={} code={} reason={detail}",
            codes::tag(codes::UPGRADE_DECLINED, code),
            offer.target.version,
            offer.upgrade_id,
            code.unwrap_or("-")
        );
        Self::report_terminal(
            inner,
            offer,
            UPGRADE_STATE_DECLINED,
            code,
            detail.to_string(),
        );
    }

    fn verify_failed(
        inner: &mut Inner,
        offer: &UpgradeOffer,
        e: &ReleaseError,
        file: &str,
        signer: &str,
    ) {
        let code = if e.code_number() == 8401 {
            codes::E_UPGRADE_SIGNATURE
        } else {
            codes::E_UPGRADE_INTEGRITY
        };
        let msg = format!(
            "{}升級中止：Pulse {} 的發佈包驗證失敗（{e}），不替換、刪除暫存  upgrade_id={} code={code} file={file} signer={signer}",
            codes::tag(codes::UPGRADE_VERIFY_FAILED, Some(code)),
            offer.target.version,
            offer.upgrade_id
        );
        log::error!("{msg}");
        if code == codes::E_UPGRADE_SIGNATURE {
            crate::service_status::attention(&msg);
        }
        let detail = sanitize_detail(&e.to_string());
        if inner
            .file
            .current
            .as_ref()
            .is_some_and(|c| c.upgrade_id.eq_ignore_ascii_case(&offer.upgrade_id))
        {
            Self::finish(inner, UPGRADE_STATE_FAILED, Some(code), detail, true);
        } else {
            Self::report_terminal(inner, offer, UPGRADE_STATE_FAILED, Some(code), detail);
        }
    }

    fn local_kind(inner: &Inner) -> Option<(Os, Arch, Format)> {
        release::artifact_kind(inner.env.detection.method, &inner.env.arch)
    }

    fn trust<'a>(inner: &'a Inner, signers: &'a AllowedSigners) -> TrustPolicy<'a> {
        TrustPolicy {
            signers,
            extra_principals: &inner.env.extra_principals,
            now_unix: chrono::Utc::now().timestamp(),
        }
    }

    /// 處理 `upgrade_offer`（REMOTE.md 第 4.2.2、4.6、4.8 節）
    pub fn on_offer(&self, cs: &mut ConnState, offer: UpgradeOffer) {
        let mut inner = self.lock();
        if cs
            .get_in_flight
            .as_deref()
            .is_some_and(|id| id.eq_ignore_ascii_case(&offer.upgrade_id))
        {
            cs.get_in_flight = None;
        }
        if inner
            .file
            .history
            .iter()
            .any(|h| h.upgrade_id.eq_ignore_ascii_case(&offer.upgrade_id))
        {
            return;
        }
        // 交付前的再確認：同一個升級的提議仍在（時段可能已更新），目標與檔案必須相同
        let local_timeout = inner.env.health_timeout_secs;
        let mut matched = None;
        if let Some(c) = inner.file.current.as_mut()
            && c.upgrade_id.eq_ignore_ascii_case(&offer.upgrade_id)
        {
            let same = c.offer.target == offer.target
                && c.offer.artifact.sha256 == offer.artifact.sha256
                && c.offer.sha256sums == offer.sha256sums
                && c.offer.signature == offer.signature
                && mgmt::validate_upgrade_offer(&offer).is_ok();
            if same {
                c.offer.apply_window = offer.apply_window.clone();
                c.offer.health_timeout_secs = offer.health_timeout_secs;
                c.health_timeout_secs =
                    mgmt::effective_health_timeout_secs(offer.health_timeout_secs, local_timeout);
            }
            matched = Some((same, c.phase == Phase::Staged));
        }
        if let Some((same, staged)) = matched {
            if same {
                if staged {
                    cs.reconfirmed = true;
                    cs.reconfirm_after = None;
                }
                Self::persist(&mut inner);
            } else {
                log::warn!(
                    "{}同一個升級 {} 的提議內容改變了，忽略",
                    codes::tag(codes::SERVER_UNEXPECTED, Some(codes::W_SERVER_UNEXPECTED)),
                    offer.upgrade_id
                );
            }
            return;
        }
        log::info!(
            "{}收到升級提議：Pulse {} → {}（推送 {}，時段 {}～{}）  upgrade_id={} from_version={} to_version={} rollout={}",
            codes::tag(codes::UPGRADE_OFFERED, None),
            inner.env.current_version,
            offer.target.version,
            sanitize_detail(&offer.rollout_id),
            offer.apply_window.not_before,
            offer.apply_window.not_after,
            offer.upgrade_id,
            inner.env.current_version,
            offer.target.version,
            sanitize_detail(&offer.rollout_id)
        );
        // 0. 格式
        if let Err(e) = mgmt::validate_upgrade_offer(&offer) {
            Self::decline(
                &mut inner,
                &offer,
                Some(codes::E_UPGRADE_INTEGRITY),
                &format!("invalid_offer: {e}"),
            );
            return;
        }
        // 1. 本機狀態
        let d = &inner.env.detection;
        if d.upgrade != mgmt::INSTALL_UPGRADE_READY {
            let code = match d.upgrade {
                mgmt::INSTALL_UPGRADE_DISABLED => codes::I_UPGRADE_DISABLED,
                mgmt::INSTALL_UPGRADE_NO_TRUST_ANCHOR => codes::E_UPGRADE_TRUST_ANCHOR,
                _ => codes::W_UPGRADE_UNSUPPORTED,
            };
            let detail = d.upgrade.to_string();
            Self::decline(&mut inner, &offer, Some(code), &detail);
            return;
        }
        // 進行中的升級
        if let Some(c) = inner.file.current.clone() {
            if matches!(c.phase, Phase::Handoff | Phase::Applying) {
                Self::decline(&mut inner, &offer, None, "upgrade_in_progress");
                return;
            }
            if c.phase == Phase::Staged
                && c.to_version == offer.target.version
                && c.files.first().map(|f| (&f.name, &f.sha256))
                    == Some((&offer.artifact.filename, &offer.artifact.sha256))
            {
                // 同一個目標版本：沿用已暫存的檔案（REMOTE.md 第 2.5 節）
                let new_id = offer.upgrade_id.to_ascii_lowercase();
                let old_dir = Self::staging_dir(&inner, &c.upgrade_id);
                let new_dir = Self::staging_dir(&inner, &new_id);
                if std::fs::rename(&old_dir, &new_dir).is_ok() {
                    let mut c2 = c.clone();
                    c2.upgrade_id = new_id;
                    c2.offer = offer;
                    c2.health_timeout_secs = mgmt::effective_health_timeout_secs(
                        c2.offer.health_timeout_secs,
                        inner.env.health_timeout_secs,
                    );
                    let total: u64 = c2.files.iter().map(|f| f.size).sum();
                    let mut accepted =
                        Self::status_for(&c2, UPGRADE_STATE_ACCEPTED, None, String::new());
                    accepted.detail = "reused_staged".into();
                    let mut staged =
                        Self::status_for(&c2, UPGRADE_STATE_STAGED, None, String::new());
                    staged.progress = Some(UpgradeProgress {
                        bytes: total,
                        total,
                    });
                    inner.file.current = Some(c2);
                    Self::push_status(&mut inner, accepted);
                    Self::push_status(&mut inner, staged);
                    Self::persist(&mut inner);
                    return;
                }
            }
            // 其他：放棄先前的（Kairos 已改提議別的升級）
            log::info!(
                "放棄先前的升級 {}（Pulse {}），改處理新的提議",
                c.upgrade_id,
                c.to_version
            );
            let _ = std::fs::remove_dir_all(Self::staging_dir(&inner, &c.upgrade_id));
            inner.file.current = None;
            Self::persist(&mut inner);
        }
        // 2. 來源（外部 URL 在 0.10.0）
        if offer.artifact.source != SOURCE_KAIROS
            || offer
                .rollback_artifact
                .as_ref()
                .is_some_and(|r| r.source != SOURCE_KAIROS)
        {
            Self::decline(
                &mut inner,
                &offer,
                Some(codes::W_UPGRADE_UNSUPPORTED),
                "url_not_allowed",
            );
            return;
        }
        // 3. 平台與格式
        let Some((os, arch, format)) = Self::local_kind(&inner) else {
            Self::decline(
                &mut inner,
                &offer,
                Some(codes::W_UPGRADE_UNSUPPORTED),
                "unsupported_platform",
            );
            return;
        };
        // 4. 空間
        let need = mgmt::staging_space_needed(offer.artifact.size);
        let updir = Self::upgrade_dir(&inner);
        if let Some(free) = (inner.env.free_space)(&updir)
            && free < need
        {
            Self::decline(
                &mut inner,
                &offer,
                Some(codes::W_UPGRADE_SPACE),
                &format!("insufficient_space: need {need} bytes, free {free} bytes"),
            );
            return;
        }
        // 5. 信任錨與簽章（第 4.2.2 節步驟 1～4）
        let signers = match install::load_trust_anchor(
            &inner.env.detection.trust_anchor,
            inner.env.require_root_owner,
        ) {
            Ok(s) => s,
            Err(e) => {
                log::error!(
                    "{}遠端升級已停用：信任錨無法使用（{e}）  path={} reason={}",
                    codes::tag(
                        codes::UPGRADE_TRUST_ANCHOR_UNAVAILABLE,
                        Some(codes::E_UPGRADE_TRUST_ANCHOR)
                    ),
                    inner.env.detection.trust_anchor.display(),
                    sanitize_detail(&e)
                );
                Self::decline(
                    &mut inner,
                    &offer,
                    Some(codes::E_UPGRADE_TRUST_ANCHOR),
                    "no_trust_anchor",
                );
                return;
            }
        };
        let bundle = ReleaseBundle {
            sha256sums: &offer.sha256sums,
            signature: &offer.signature,
            manifest: offer
                .manifest
                .as_ref()
                .map(|m| (m.filename.as_str(), m.content.as_str())),
        };
        let verified = match release::verify_bundle(&bundle, &Self::trust(&inner, &signers)) {
            Ok(v) => v,
            Err(e) => {
                Self::verify_failed(&mut inner, &offer, &e, "SHA256SUMS", "-");
                return;
            }
        };
        let expect = ArtifactExpectation {
            product: Some(Product::Pulse),
            version: Some(&offer.target.version),
            commit: Some(&offer.target.commit),
            os: Some(os),
            arch: Some(arch),
            format: Some(format),
        };
        if let Err(e) =
            verified.check_artifact(&offer.artifact.filename, &offer.artifact.sha256, &expect)
        {
            let signer = verified.signer.fingerprint.clone();
            Self::verify_failed(&mut inner, &offer, &e, &offer.artifact.filename, &signer);
            return;
        }
        // 6. 版本政策（第 4.8 節）
        let current = Version::parse(&inner.env.current_version);
        let target = Version::parse(&offer.target.version);
        let (Ok(current), Ok(target)) = (current, target) else {
            Self::decline(
                &mut inner,
                &offer,
                Some(codes::E_UPGRADE_INTEGRITY),
                "invalid_version",
            );
            return;
        };
        if let Err(p) = release::check_version_policy(
            &current,
            &target,
            verified.manifest.as_ref(),
            offer.allow_downgrade,
            inner.env.allow_downgrade,
        ) {
            let code = match p {
                PolicyDecline::DowngradeNotAllowed => codes::W_UPGRADE_DOWNGRADE,
                _ => codes::W_UPGRADE_UNSUPPORTED,
            };
            let detail = match &p {
                PolicyDecline::BelowMinUpgradeFrom { min_upgrade_from } => {
                    format!("min_upgrade_from: {min_upgrade_from}")
                }
                other => other.detail().to_string(),
            };
            Self::decline(&mut inner, &offer, Some(code), &detail);
            return;
        }
        if offer.action == ACTION_DOWNGRADE && target >= current {
            log::debug!("提議的 action 為 downgrade，但目標不低於目前版本，依版本比較處理");
        }
        // 7. 回滾檔：只有本機有它所屬發佈的驗證紀錄時才使用
        let rollback_trusted = offer
            .rollback_artifact
            .as_ref()
            .is_some_and(|r| Self::rollback_is_trusted(&inner, &signers, r, os, arch, format));
        if offer.rollback_artifact.is_some() && !rollback_trusted {
            log::info!(
                "提議附的回滾檔 {} 所屬的發佈在本機沒有驗證過的 SHA256SUMS：不下載，自動回滾時改以替換前保留的執行檔回復",
                offer
                    .rollback_artifact
                    .as_ref()
                    .map_or("", |r| r.filename.as_str())
            );
        }
        // 接受
        let id = offer.upgrade_id.to_ascii_lowercase();
        let mut files = vec![FileItem {
            name: offer.artifact.filename.clone(),
            size: offer.artifact.size,
            sha256: offer.artifact.sha256.clone(),
            done: false,
        }];
        if rollback_trusted && let Some(r) = &offer.rollback_artifact {
            files.push(FileItem {
                name: r.filename.clone(),
                size: r.size,
                sha256: r.sha256.clone(),
                done: false,
            });
        }
        let dir = Self::staging_dir(&inner, &id);
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = crate::remote::create_dir_all_mode(&dir, crate::remote::DIR_MODE) {
            Self::decline(
                &mut inner,
                &offer,
                Some(codes::W_UPGRADE_SPACE),
                &format!("staging_unwritable: {e}"),
            );
            return;
        }
        let c = Current {
            upgrade_id: id,
            from_version: inner.env.current_version.clone(),
            to_version: offer.target.version.clone(),
            method: inner.env.detection.method.to_string(),
            files,
            mismatches: 0,
            quarter: 0,
            health_timeout_secs: mgmt::effective_health_timeout_secs(
                offer.health_timeout_secs,
                inner.env.health_timeout_secs,
            ),
            rollback_trusted,
            accepted_at: crate::protocol::now_ts(),
            applying_at: None,
            phase: Phase::Downloading,
            offer,
        };
        let accepted = Self::status_for(&c, UPGRADE_STATE_ACCEPTED, None, String::new());
        inner.file.current = Some(c);
        Self::push_status(&mut inner, accepted);
        Self::persist(&mut inner);
    }

    /// 回滾檔所屬的發佈（目前的版本）在本機有驗證過的 `SHA256SUMS`，且回滾檔列在其中
    fn rollback_is_trusted(
        inner: &Inner,
        signers: &AllowedSigners,
        r: &ArtifactRef,
        os: Os,
        arch: Arch,
        format: Format,
    ) -> bool {
        let dir = Self::upgrade_dir(inner)
            .join(RELEASES_DIR)
            .join(&inner.env.current_version);
        let (Ok(sums), Ok(sig)) = (
            std::fs::read_to_string(dir.join(files::SUMS_FILE)),
            std::fs::read_to_string(dir.join(files::SIG_FILE)),
        ) else {
            return false;
        };
        let bundle = ReleaseBundle {
            sha256sums: &sums,
            signature: &sig,
            manifest: None,
        };
        let Ok(v) = release::verify_bundle(&bundle, &Self::trust(inner, signers)) else {
            return false;
        };
        let expect = ArtifactExpectation {
            product: Some(Product::Pulse),
            version: Some(&inner.env.current_version),
            commit: None,
            os: Some(os),
            arch: Some(arch),
            format: Some(format),
        };
        v.check_artifact(&r.filename, &r.sha256, &expect).is_ok()
    }

    /// 處理 `upgrade_chunk`
    pub fn on_chunk(
        &self,
        cs: &mut ConnState,
        chunk: &UpgradeChunk,
        requested: (String, u64, u32),
    ) {
        cs.chunks.retain(|x| *x != requested);
        let mut inner = self.lock();
        let Some(c) = inner.file.current.clone() else {
            return;
        };
        if c.phase != Phase::Downloading || !chunk.upgrade_id.eq_ignore_ascii_case(&c.upgrade_id) {
            return;
        }
        let (name, offset, length) = requested;
        let Some(idx) = c.files.iter().position(|f| f.name == name && !f.done) else {
            return;
        };
        let file = c.files[idx].clone();
        let dir = Self::staging_dir(&inner, &c.upgrade_id);
        let part = dir.join(format!("{name}.part"));
        let on_disk = std::fs::metadata(&part).map_or(0, |m| m.len());
        if chunk.filename != name || chunk.offset != offset || offset != on_disk {
            cs.resync = true;
            return;
        }
        let data = match chunk.decode_data() {
            Ok(d) => d,
            Err(e) => {
                Self::log_download(&mut inner, &c, &format!("段落無法解碼：{e}"));
                cs.resync = true;
                return;
            }
        };
        let end = offset + data.len() as u64;
        let complete = end == file.size;
        let bad = data.len() > length as usize
            || end > file.size
            || (chunk.eof && !complete)
            || (data.is_empty() && !complete);
        if bad {
            Self::log_download(
                &mut inner,
                &c,
                &format!(
                    "段落的長度不符（offset {offset}、{} bytes、eof={}，檔案 {} bytes），從頭重新下載 {name}",
                    data.len(),
                    chunk.eof,
                    file.size
                ),
            );
            let _ = std::fs::remove_file(&part);
            cs.resync = true;
            return;
        }
        let write = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&part)?;
            f.write_all(&data)?;
            if complete {
                f.sync_all()?;
            }
            Ok(())
        })();
        if let Err(e) = write {
            Self::log_download(
                &mut inner,
                &c,
                &format!("無法寫入暫存檔 {}：{e}", part.display()),
            );
            cs.retry_until = Some(Instant::now() + Duration::from_secs(30));
            return;
        }
        // 進度（每 25%）
        let (done, total) = Self::progress(&inner, &c);
        let quarter = u8::try_from(done.saturating_mul(4) / total.max(1)).unwrap_or(4);
        if quarter > c.quarter && quarter < 4 {
            let mut r = Self::status_for(&c, UPGRADE_STATE_ACCEPTED, None, String::new());
            r.progress = Some(UpgradeProgress { bytes: done, total });
            if let Some(cur) = inner.file.current.as_mut() {
                cur.quarter = quarter;
            }
            Self::push_status(&mut inner, r);
            Self::persist(&mut inner);
        }
        if !complete {
            return;
        }
        // 一個檔案下載完成：以提議的 SHA-256 檢查
        let actual = files::sha256_file(&part).unwrap_or_default();
        if actual != file.sha256 {
            let _ = std::fs::remove_file(&part);
            let mismatches = c.mismatches + 1;
            if let Some(cur) = inner.file.current.as_mut() {
                cur.mismatches = mismatches;
            }
            if mismatches >= MAX_DOWNLOAD_HASH_MISMATCHES {
                let e = ReleaseError::Integrity(format!(
                    "SHA-256 of {name} did not match {MAX_DOWNLOAD_HASH_MISMATCHES} times"
                ));
                let offer = c.offer.clone();
                Self::verify_failed(&mut inner, &offer, &e, &name, "-");
            } else {
                Self::log_download(
                    &mut inner,
                    &c,
                    &format!("{name} 的 SHA-256 與提議不符（第 {mismatches} 次），刪除後重新下載"),
                );
                Self::persist(&mut inner);
            }
            return;
        }
        if let Err(e) = std::fs::rename(&part, dir.join(&name)) {
            Self::log_download(&mut inner, &c, &format!("無法改名暫存檔：{e}"));
            return;
        }
        if let Some(cur) = inner.file.current.as_mut() {
            cur.files[idx].done = true;
        }
        Self::persist(&mut inner);
        if inner
            .file
            .current
            .as_ref()
            .is_some_and(|c| c.files.iter().all(|f| f.done))
        {
            Self::stage(&mut inner);
        }
    }

    fn log_download(inner: &mut Inner, c: &Current, what: &str) {
        if let Some(n) = inner.download_log.check(Instant::now()) {
            log::warn!(
                "{}下載 Pulse {} 失敗，續傳重試中：{what}{}  upgrade_id={} error={} attempt={}",
                codes::tag(
                    codes::UPGRADE_DOWNLOAD_FAILED,
                    Some(codes::W_UPGRADE_DOWNLOAD)
                ),
                c.to_version,
                crate::log_throttle::suppressed_note(n, DOWNLOAD_LOG_WINDOW),
                c.upgrade_id,
                sanitize_detail(what),
                c.mismatches + 1
            );
        }
    }

    /// 全部下載完成：重新驗證、寫入簽章資料與清單、保存驗證過的發佈紀錄 → `staged`
    fn stage(inner: &mut Inner) {
        let Some(c) = inner.file.current.clone() else {
            return;
        };
        let dir = Self::staging_dir(inner, &c.upgrade_id);
        let offer = &c.offer;
        let signers = match install::load_trust_anchor(
            &inner.env.detection.trust_anchor,
            inner.env.require_root_owner,
        ) {
            Ok(s) => s,
            Err(e) => {
                let err = ReleaseError::Signature(release::SigError::UnknownSigner);
                log::error!("信任錨在下載期間變得無法使用：{e}");
                Self::verify_failed(inner, offer, &err, "allowed_signers", "-");
                return;
            }
        };
        let bundle = ReleaseBundle {
            sha256sums: &offer.sha256sums,
            signature: &offer.signature,
            manifest: offer
                .manifest
                .as_ref()
                .map(|m| (m.filename.as_str(), m.content.as_str())),
        };
        let verified = match release::verify_bundle(&bundle, &Self::trust(inner, &signers)) {
            Ok(v) => v,
            Err(e) => {
                Self::verify_failed(inner, offer, &e, "SHA256SUMS", "-");
                return;
            }
        };
        let actual = files::sha256_file(&dir.join(&offer.artifact.filename)).unwrap_or_default();
        let (os, arch, format) = match Self::local_kind(inner) {
            Some(k) => k,
            None => return,
        };
        let expect = ArtifactExpectation {
            product: Some(Product::Pulse),
            version: Some(&offer.target.version),
            commit: Some(&offer.target.commit),
            os: Some(os),
            arch: Some(arch),
            format: Some(format),
        };
        if let Err(e) = verified.check_artifact(&offer.artifact.filename, &actual, &expect) {
            let signer = verified.signer.fingerprint.clone();
            Self::verify_failed(inner, offer, &e, &offer.artifact.filename, &signer);
            return;
        }
        let write = |name: &str, content: &[u8]| {
            crate::remote::write_atomic(
                &dir.join(name),
                content,
                crate::remote::FILE_MODE,
                crate::remote::DIR_MODE,
            )
        };
        let mut result = write(files::SUMS_FILE, offer.sha256sums.as_bytes())
            .and_then(|()| write(files::SIG_FILE, offer.signature.as_bytes()));
        if let Some(m) = &offer.manifest {
            result = result.and_then(|()| write(&m.filename, m.content.as_bytes()));
        }
        if c.rollback_trusted {
            let rel = Self::upgrade_dir(inner)
                .join(RELEASES_DIR)
                .join(&c.from_version);
            for (src, dst) in [
                (files::SUMS_FILE, files::ROLLBACK_SUMS_FILE),
                (files::SIG_FILE, files::ROLLBACK_SIG_FILE),
            ] {
                result = result.and_then(|()| {
                    let b = std::fs::read(rel.join(src))?;
                    write(dst, &b)
                });
            }
        }
        // 驗證過的發佈：日後作為回滾檔的信任來源
        let rel = Self::upgrade_dir(inner)
            .join(RELEASES_DIR)
            .join(&c.to_version);
        let saved = crate::remote::write_atomic(
            &rel.join(files::SUMS_FILE),
            offer.sha256sums.as_bytes(),
            crate::remote::FILE_MODE,
            crate::remote::DIR_MODE,
        )
        .and_then(|()| {
            crate::remote::write_atomic(
                &rel.join(files::SIG_FILE),
                offer.signature.as_bytes(),
                crate::remote::FILE_MODE,
                crate::remote::DIR_MODE,
            )
        });
        if let Err(e) = saved {
            log::debug!("無法保存驗證過的發佈紀錄 {}：{e}", rel.display());
        }
        Self::prune_releases(inner, &c.to_version);
        if let Err(e) = result {
            let e = ReleaseError::Integrity(format!("cannot write staging files: {e}"));
            Self::verify_failed(inner, offer, &e, "staging", "-");
            return;
        }
        let total: u64 = c.files.iter().map(|f| f.size).sum();
        log::info!(
            "{}Pulse {} 已下載並通過第一次驗證（簽署者 {}），已暫存，等待升級時段 {}～{}  upgrade_id={} bytes={total} duration_ms={}",
            codes::tag(codes::UPGRADE_DOWNLOADED, None),
            c.to_version,
            verified.signer.fingerprint,
            offer.apply_window.not_before,
            offer.apply_window.not_after,
            c.upgrade_id,
            kairos_proto::enroll::parse_rfc3339_ms(&c.accepted_at)
                .map_or(0, |t| (now_ms() - t).max(0))
        );
        let mut r = Self::status_for(&c, UPGRADE_STATE_STAGED, None, String::new());
        r.progress = Some(UpgradeProgress {
            bytes: total,
            total,
        });
        if let Some(cur) = inner.file.current.as_mut() {
            cur.phase = Phase::Staged;
        }
        Self::push_status(inner, r);
        Self::persist(inner);
    }

    /// 驗證過的發佈紀錄只保留目前與目標版本
    fn prune_releases(inner: &Inner, keep: &str) {
        let dir = Self::upgrade_dir(inner).join(RELEASES_DIR);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name != keep && name != inner.env.current_version {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    fn start_handoff(inner: &mut Inner) {
        let Some(c) = inner.file.current.as_mut() else {
            return;
        };
        c.phase = Phase::Handoff;
        c.applying_at = Some(crate::protocol::now_ts());
        let c = c.clone();
        log::info!(
            "{}進入升級時段，交給 pulse-upgrade.service 替換為 Pulse {}（{}；健康確認 {} 秒）  upgrade_id={} method={}",
            codes::tag(codes::UPGRADE_APPLYING, None),
            c.to_version,
            c.method,
            c.health_timeout_secs,
            c.upgrade_id,
            c.method
        );
        let r = Self::status_for(&c, UPGRADE_STATE_APPLYING, None, String::new());
        Self::push_status(inner, r);
        Self::persist(inner);
    }

    /// 寫 `request.json`（寫暫存檔再改名；觸發 `pulse-upgrade.path`）
    fn write_request(inner: &mut Inner) {
        let Some(c) = inner.file.current.clone() else {
            return;
        };
        if c.phase != Phase::Handoff {
            return;
        }
        let req = Request {
            schema: files::REQUEST_SCHEMA.to_string(),
            upgrade_id: c.upgrade_id.clone(),
            target: c.offer.target.clone(),
            method: c.method.clone(),
            artifact: c.offer.artifact.filename.clone(),
            manifest: c.offer.manifest.as_ref().map(|m| m.filename.clone()),
            rollback_artifact: c
                .offer
                .rollback_artifact
                .as_ref()
                .filter(|_| c.rollback_trusted)
                .map(|r| r.filename.clone()),
            health_timeout_secs: c.health_timeout_secs,
            allow_downgrade: c.offer.allow_downgrade,
            from_version: c.from_version.clone(),
            requested_at: crate::protocol::now_ts(),
        };
        let path = Self::upgrade_dir(inner).join(files::REQUEST_FILE);
        let bytes = serde_json::to_vec_pretty(&req).unwrap_or_default();
        // request.json 由 root 的輔助程式讀取；0644 讓 root 以外也能看（不含秘密）
        match crate::remote::write_atomic(&path, &bytes, 0o644, crate::remote::DIR_MODE) {
            Ok(()) => {
                if let Some(cur) = inner.file.current.as_mut() {
                    cur.phase = Phase::Applying;
                }
                Self::persist(inner);
                log::info!("已寫入 {}，等待 pulse-upgrade.service 執行", path.display());
            }
            Err(e) => {
                let msg = format!("cannot write request.json: {e}");
                log::error!(
                    "{}無法交給輔助程式：{msg}  upgrade_id={} code={} detail={msg}",
                    codes::tag(codes::UPGRADE_FAILED, Some(codes::E_UPGRADE_REPLACE)),
                    c.upgrade_id,
                    codes::E_UPGRADE_REPLACE
                );
                Self::finish(
                    inner,
                    UPGRADE_STATE_FAILED,
                    Some(codes::E_UPGRADE_REPLACE),
                    msg,
                    true,
                );
            }
        }
    }

    /// 讀 `result.json`（替換中，或重新啟動後）：放進 outbox 並記錄
    fn poll_result(inner: &mut Inner) {
        let Some(c) = inner.file.current.clone() else {
            return;
        };
        if c.phase != Phase::Applying {
            return;
        }
        let path = Self::upgrade_dir(inner).join(files::RESULT_FILE);
        let result = match UpgradeResult::read(&path) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                log::warn!("{e}");
                return;
            }
        };
        if !result.upgrade_id.eq_ignore_ascii_case(&c.upgrade_id) {
            log::debug!(
                "{} 屬於另一個升級（{}），忽略",
                path.display(),
                result.upgrade_id
            );
            return;
        }
        let code = result.code.as_deref().filter(|c| mgmt::is_product_code(c));
        let detail = sanitize_detail(&result.detail);
        match result.state.as_str() {
            UPGRADE_STATE_SUCCEEDED => log::info!(
                "{}已升級到 Pulse {}（健康確認通過）  upgrade_id={} from_version={} to_version={} duration_ms={}",
                codes::tag(codes::UPGRADE_COMPLETED, Some(codes::I_UPGRADE_COMPLETED)),
                result.to_version,
                c.upgrade_id,
                result.from_version,
                result.to_version,
                c.applying_at
                    .as_deref()
                    .and_then(|t| kairos_proto::enroll::parse_rfc3339_ms(t).ok())
                    .map_or(0, |t| (now_ms() - t).max(0))
            ),
            UPGRADE_STATE_ROLLED_BACK => log::warn!(
                "{}升級到 Pulse {} 後沒有恢復正常，已自動回滾到 {}（{detail}）  upgrade_id={} failed_check={detail} restored_version={}",
                codes::tag(codes::UPGRADE_ROLLED_BACK, code),
                result.to_version,
                result.from_version,
                c.upgrade_id,
                result.from_version
            ),
            UPGRADE_STATE_ROLLBACK_FAILED => {
                let msg = format!(
                    "{}升級到 Pulse {} 後回滾也失敗，需要人工處理（{detail}；紀錄在 /var/lib/pulse-upgrade/{}/）  upgrade_id={} staging_dir=/var/lib/pulse-upgrade/{} reasons={detail}",
                    codes::tag(codes::UPGRADE_ROLLBACK_FAILED, code),
                    result.to_version,
                    c.upgrade_id,
                    c.upgrade_id,
                    c.upgrade_id
                );
                log::error!("{msg}");
                crate::service_status::attention(&msg);
            }
            _ => log::error!(
                "{}升級到 Pulse {} 失敗（{detail}）  upgrade_id={} code={} detail={detail}",
                codes::tag(codes::UPGRADE_FAILED, code),
                result.to_version,
                c.upgrade_id,
                code.unwrap_or("-")
            ),
        }
        let state = match result.state.as_str() {
            s @ (UPGRADE_STATE_SUCCEEDED
            | UPGRADE_STATE_ROLLED_BACK
            | UPGRADE_STATE_ROLLBACK_FAILED) => s,
            _ => UPGRADE_STATE_FAILED,
        };
        Self::finish(inner, state, code, detail, true);
        let _ = std::fs::remove_file(&path);
    }

    /// 我們送出的請求收到 `error`（非 fatal）
    pub fn on_error(&self, cs: &mut ConnState, out: &Out, code: &str, retry_after_ms: Option<u32>) {
        let mut inner = self.lock();
        match out {
            Out::Get(id) => {
                cs.get_in_flight = None;
                if code == "upgrade_unavailable" {
                    let staged = inner
                        .file
                        .current
                        .as_ref()
                        .is_some_and(|c| c.upgrade_id.eq_ignore_ascii_case(id));
                    if staged {
                        // 交付前的再確認被拒：推送已暫停或取消，先不替換（保留暫存，之後再確認或等同一版的新提議）
                        log::info!(
                            "升級 {id} 的推送已暫停或取消（upgrade_unavailable），暫不替換；保留已暫存的檔案"
                        );
                    } else {
                        log::debug!("升級 {id} 已無法取得（upgrade_unavailable）");
                        cs.unavailable.push(id.clone());
                    }
                }
            }
            Out::Chunk {
                filename,
                offset,
                length,
                ..
            } => {
                cs.chunks
                    .retain(|x| *x != (filename.clone(), *offset, *length));
                match code {
                    "rate_limited" => {
                        let wait =
                            Duration::from_millis(u64::from(retry_after_ms.unwrap_or(30_000)));
                        cs.retry_until = Some(Instant::now() + jitter(wait));
                        cs.resync = true;
                        log::debug!("Kairos 的下載併發已滿，{} 毫秒後續傳", wait.as_millis());
                    }
                    "upgrade_unavailable" => {
                        if let Some(c) = inner.file.current.clone()
                            && c.phase == Phase::Downloading
                        {
                            log::info!(
                                "升級 {}（Pulse {}）已被取消或暫停（upgrade_unavailable），刪除暫存",
                                c.upgrade_id,
                                c.to_version
                            );
                            let _ =
                                std::fs::remove_dir_all(Self::staging_dir(&inner, &c.upgrade_id));
                            cs.unavailable.push(c.upgrade_id.clone());
                            inner.file.current = None;
                            Self::persist(&mut inner);
                        }
                    }
                    _ => {
                        cs.retry_until = Some(Instant::now() + Duration::from_secs(30));
                        cs.resync = true;
                        if let Some(c) = inner.file.current.clone() {
                            Self::log_download(&mut inner, &c, &format!("Kairos 回應 {code}"));
                        }
                    }
                }
            }
            Out::Status(r) => {
                cs.status_in_flight.retain(|k| *k != r.key());
                if code == "storage_unavailable" {
                    cs.status_retry_until = Some(Instant::now() + STATUS_RETRY);
                } else {
                    Self::status_failed(&mut inner, r);
                }
            }
        }
    }

    fn status_failed(inner: &mut Inner, r: &StatusRecord) {
        if let Some(pos) = inner.file.outbox.iter().position(|x| x.key() == r.key()) {
            inner.file.outbox[pos].attempts += 1;
            if inner.file.outbox[pos].attempts >= MAX_STATUS_ATTEMPTS {
                log::warn!(
                    "{}Kairos 不接受升級狀態 {}（{}），放棄回報",
                    codes::tag(codes::SERVER_UNEXPECTED, Some(codes::W_SERVER_UNEXPECTED)),
                    r.state,
                    r.upgrade_id
                );
                inner.file.outbox.remove(pos);
            }
            Self::persist(inner);
        }
    }

    /// 我們送出的請求沒有收到回應（之後的訊息已被回應，或伺服器回了 `seq: null` 的錯誤）
    pub fn on_lost(&self, cs: &mut ConnState, out: &Out) {
        match out {
            Out::Get(_) => cs.get_in_flight = None,
            Out::Chunk {
                filename,
                offset,
                length,
                ..
            } => {
                cs.chunks
                    .retain(|x| *x != (filename.clone(), *offset, *length));
                cs.resync = true;
            }
            Out::Status(r) => {
                cs.status_in_flight.retain(|k| *k != r.key());
                let mut inner = self.lock();
                Self::status_failed(&mut inner, r);
            }
        }
    }

    /// `upgrade_status` 收到 `ack`
    pub fn on_status_acked(&self, cs: &mut ConnState, r: &StatusRecord) {
        cs.status_in_flight.retain(|k| *k != r.key());
        let mut inner = self.lock();
        if let Some(pos) = inner.file.outbox.iter().position(|x| x.key() == r.key()) {
            inner.file.outbox.remove(pos);
            Self::persist(&mut inner);
        }
        if r.state == UPGRADE_STATE_APPLYING
            && inner
                .file
                .current
                .as_ref()
                .is_some_and(|c| c.phase == Phase::Handoff && c.upgrade_id == r.upgrade_id)
        {
            Self::write_request(&mut inner);
            cs.next_result_poll = Some(Instant::now() + RESULT_POLL);
        }
    }

    /// `pulse status`／`pulse upgrade status` 用的摘要
    pub fn view(&self) -> UpgradeView {
        let inner = self.lock();
        let mut v = UpgradeView {
            method: inner.env.detection.method.to_string(),
            readiness: inner.env.detection.upgrade.to_string(),
            reason: inner.env.detection.reason.clone(),
            last: inner.file.history.last().cloned(),
            ..Default::default()
        };
        if let Some(c) = &inner.file.current {
            let (done, total) = Self::progress(&inner, c);
            v.phase = Some(
                match c.phase {
                    Phase::Downloading => "downloading",
                    Phase::Staged => "staged",
                    Phase::Handoff => "handoff",
                    Phase::Applying => "applying",
                }
                .to_string(),
            );
            v.upgrade_id = Some(c.upgrade_id.clone());
            v.to_version = Some(c.to_version.clone());
            v.bytes = Some(done);
            v.total = Some(total);
            v.not_before = Some(c.offer.apply_window.not_before.clone());
        }
        v
    }
}

/// 讀 `state.json` 的摘要（`pulse upgrade status`，不執行代理）
pub fn read_state(state_dir: &Path) -> Result<StateFile, String> {
    StateFile::load(&files::upgrade_dir(state_dir).join(files::STATE_FILE))
}

/// `detail`：字元限制同 `hostname`，≤ 512 bytes
pub fn truncate_detail(s: &str) -> String {
    let mut out = sanitize_detail(s);
    if out.len() > mgmt::MAX_UPGRADE_DETAIL_BYTES {
        let mut end = mgmt::MAX_UPGRADE_DETAIL_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// 把禁止的字元換成 `?`（`hostname` 的字元限制）
pub fn sanitize_detail(s: &str) -> String {
    s.chars()
        .map(|c| {
            if crate::protocol::is_forbidden_display_char(c) {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// 升級的檔名對應的格式（`tar` → `tar.gz` 等）；給輔助程式與測試用
pub fn method_format(method: &str) -> Option<Format> {
    match method {
        "tar" => Some(Format::TarGz),
        "deb" => Some(Format::Deb),
        "rpm" => Some(Format::Rpm),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
