//! 特權輔助程式 `pulse upgrade apply --request <檔案>`（`crates/kairos-proto/REMOTE.md` 第 4.4.1～4.5 節）。
//!
//! 由 root 的 `pulse-upgrade.service`（`pulse-upgrade.path` 在 `request.json` 出現時啟動）以**目前已安裝的**
//! 執行檔執行，不連網路。步驟：
//!
//! 1. 讀 `request.json`，把它與暫存的檔案**複製**到 root 專用的 `/var/lib/pulse-upgrade/<upgrade_id>/`（0700；
//!    只接受一般檔案、`O_NOFOLLOW`、`st_nlink == 1`、大小上限），之後只對複本操作。
//! 2. 重新驗證（信任錨與 `[upgrade]` 只從 root 擁有的 `pulse.toml` 讀，不讀環境變數或遠端設定快取）：
//!    簽章、`SHA256SUMS`、檔名與平台、版本政策。
//! 3. 預檢：把新的執行檔解到 `/opt/pulse/.upgrade-<id>/`，以 `pulse` 帳號執行 `pulse --version`（必須是目標版本）
//!    與 `pulse check-config --json --with-remote`（結束代碼 0／8）。不通過：`PLS-E8405`，沒有變更。
//! 4. 替換：tar 為 `pulse.prev` ＋ `rename`（unit 沒被改過時一併換新並 `daemon-reload`）；deb 為
//!    `dpkg -i --force-confold`、rpm 為 `rpm -U`（套件資料庫被鎖住時每 30 秒重試、最多 10 分鐘）。
//!    任何方式都先保留 `pulse.prev`。`systemctl restart pulse.service`。
//! 5. 健康確認（`health_timeout_secs`）：服務 `active`、`NRestarts` 沒有增加、狀態檔由新的行程寫入（`pid`、
//!    版本相符）、已註冊且有 report 被確認（`last_ack_at` 晚於重新啟動），再穩定 60 秒。結束代碼 2／3／4
//!    或崩潰重啟立即判定失敗。
//! 6. 失敗則回滾：tar 換回 `pulse.prev`（與 unit）；deb／rpm 有驗證過的回滾檔時以套件管理員裝回，否則換回
//!    `pulse.prev`（套件資料庫仍為新版，`detail` 註明）。以 `max(期限, 300 秒)` 確認舊版恢復：成功
//!    `rolled_back`（`PLS-W8407`），失敗 `rollback_failed`（`PLS-E8408`，保留所有紀錄，不再動作）。
//! 7. 結果寫 `<狀態目錄>/upgrade/result.json`，刪除 `request.json` 與暫存（`rollback_failed` 時保留暫存）。
//!
//! 每一步記在 `/var/lib/pulse-upgrade/<id>/state.json`：重新開機後 `pulse-upgrade.path` 再次觸發時接續
//! （已替換 → 重新做健康確認；期限已過 → 回滾）。外部程式只有固定路徑的 `systemctl`、`dpkg`、`dpkg-deb`、
//! `rpm`、`rpm2cpio`、`cpio`、`tar`，參數由程式組成（[`System`]）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kairos_proto::mgmt::{
    UPGRADE_STATE_FAILED, UPGRADE_STATE_ROLLBACK_FAILED, UPGRADE_STATE_ROLLED_BACK,
    UPGRADE_STATE_SUCCEEDED,
};
use kairos_proto::release::{
    self, Arch, ArtifactExpectation, Os, PolicyDecline, Product, ReleaseBundle, ReleaseManifest,
    TrustPolicy, Version,
};
use serde::{Deserialize, Serialize};

use super::files::{self, Request, UpgradeResult};
use crate::codes;
use crate::config_manager::AppConfig;

/// 輔助程式的工作目錄（root 專用）
pub const WORK_ROOT: &str = "/var/lib/pulse-upgrade";
/// 預設的 `request.json`（`pulse-upgrade.path` 的 `PathExists=`、`pulse-upgrade.service` 的 `--request`）
#[cfg(all(test, unix))]
pub const DEFAULT_REQUEST: &str = "/var/lib/pulse/upgrade/request.json";
/// tar 安裝的 systemd unit
pub const UNIT_PATH: &str = "/etc/systemd/system/pulse.service";
/// 服務帳號
#[cfg_attr(not(unix), allow(dead_code))]
pub const SERVICE_USER: &str = "pulse";
/// 符合健康條件後還要穩定多久
pub const STABLE_FOR: Duration = Duration::from_secs(60);
/// 回滾後確認舊版恢復的最短期限
pub const MIN_ROLLBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// 健康確認的輪詢間隔
const POLL: Duration = Duration::from_secs(2);
/// 套件資料庫被鎖住時的重試間隔與上限（REMOTE.md 第 4.4.1 節）
const LOCK_RETRY: Duration = Duration::from_secs(30);
const LOCK_GIVE_UP: Duration = Duration::from_secs(600);
/// 簽章資料的大小上限
const MAX_SUMS: u64 = kairos_proto::mgmt::MAX_SHA256SUMS_BYTES as u64;
const MAX_SIG: u64 = kairos_proto::mgmt::MAX_SIGNATURE_BYTES as u64;
const MAX_MANIFEST: u64 = kairos_proto::mgmt::MAX_MANIFEST_BYTES as u64;
/// 輔助程式狀態檔的 schema
const HELPER_SCHEMA: &str = "pulse.upgrade-helper/v1";

/// 服務的狀態（`systemctl show`）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceState {
    /// `ActiveState`
    pub active: String,
    /// `NRestarts`
    pub n_restarts: u64,
    /// `MainPID`
    pub main_pid: u32,
    /// `ExecMainStatus`（結束代碼）
    pub exit_status: Option<i32>,
}

/// 外部程式的結果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// `None`：無法執行或被訊號結束
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    fn ok(&self) -> bool {
        self.status == Some(0)
    }

    /// stderr（沒有時 stdout）的最後一行非空白文字（`detail` 用）
    fn last_line(&self) -> String {
        let pick = |s: &str| {
            s.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(str::trim)
                .map(str::to_string)
        };
        pick(&self.stderr)
            .or_else(|| pick(&self.stdout))
            .unwrap_or_else(|| format!("exit status {:?}", self.status))
    }
}

/// 要執行的外部程式（固定的程式，參數由程式組成）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// `tar -xzf <封存> -C <目錄> --no-same-owner`
    TarExtract { archive: PathBuf, dir: PathBuf },
    /// `dpkg-deb -x <套件> <目錄>`
    DebExtract { package: PathBuf, dir: PathBuf },
    /// `rpm2cpio <套件> | cpio -idm`（在目錄中）
    RpmExtract { package: PathBuf, dir: PathBuf },
    /// `dpkg -i --force-confold <套件>`（`DEBIAN_FRONTEND=noninteractive`）
    DpkgInstall { package: PathBuf },
    /// `rpm -U --oldpackage --replacepkgs <套件>`：與 `dpkg -i` 一樣允許降版與重新安裝同一版。版本政策已由
    /// 輔助程式依**實際的執行檔**判斷（REMOTE.md 第 4.8 節）；套件資料庫可能與執行檔不符（沒有回滾檔時
    /// 以 `pulse.prev` 回滾，資料庫仍是失敗的那一版），不加這兩個選項的話，之後重試同一版或推送比它低的
    /// 版本都會被 rpm 拒絕（「already installed」「newer … is already installed」）
    RpmInstall { package: PathBuf },
    /// 以服務帳號執行解開的執行檔（`--version`、`check-config …`），工作目錄為安裝目錄
    RunAsService {
        program: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        state_dir: PathBuf,
    },
}

/// 與作業系統互動的部分（測試時以假的實作取代：不需要 root、systemd 或真的等待）
pub trait System {
    /// 單調時鐘
    fn mono(&self) -> Duration;
    /// 牆上時鐘（Unix 秒；健康確認的 `last_ack_at` 比較、信任錨的有效期間）
    fn now_unix(&self) -> i64;
    fn sleep(&self, d: Duration);
    fn restart_service(&self) -> Result<(), String>;
    fn service_state(&self) -> Result<ServiceState, String>;
    fn daemon_reload(&self) -> Result<(), String>;
    fn run(&self, cmd: &Cmd) -> Output;
}

/// 路徑（正式環境為固定位置；測試時以暫存目錄模擬）
#[derive(Debug, Clone)]
pub struct Paths {
    pub request: PathBuf,
    pub install_dir: PathBuf,
    pub work_root: PathBuf,
    pub unit_path: PathBuf,
    /// 信任錨必須由 root 擁有（測試時 `false`）
    pub require_root_owner: bool,
    /// 這個輔助程式的版本（＝目前安裝的版本）
    pub current_version: String,
    /// 輔助程式自己偵測到的安裝方式（`tar`／`deb`／`rpm`／`unknown`；不採用 `request.json` 的 `method`）
    pub method: String,
    pub arch: String,
}

impl Paths {
    /// 正式環境：安裝目錄為執行檔所在的目錄（`/opt/pulse`）
    pub fn system(request: PathBuf) -> Self {
        let install_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from(super::super::install::INSTALL_DIR));
        Self {
            request,
            install_dir,
            work_root: PathBuf::from(WORK_ROOT),
            unit_path: PathBuf::from(UNIT_PATH),
            require_root_owner: true,
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            method:
                crate::remote::install::detect_method(&crate::remote::install::Probe::for_cli())
                    .to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    fn upgrade_dir(&self) -> PathBuf {
        self.request
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    fn state_dir(&self) -> PathBuf {
        self.upgrade_dir()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    fn result_path(&self) -> PathBuf {
        self.upgrade_dir().join(files::RESULT_FILE)
    }
}

/// 輔助程式的步驟（`/var/lib/pulse-upgrade/<id>/state.json`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// 已複製並驗證、預檢通過，尚未替換
    Verified,
    /// 已替換、已重新啟動，健康確認中
    Replaced,
    /// 回滾中（健康確認舊版）
    RollingBack,
    /// 完成（結果已寫出）
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HelperState {
    schema: String,
    step: Step,
    /// 健康確認的期限（Unix 秒）
    deadline_unix: i64,
    /// 重新啟動前的 `NRestarts`
    n_restarts_before: u64,
    /// 重新啟動的時間（Unix 秒）
    restarted_unix: i64,
    /// 實際使用的健康確認期限（秒）
    #[serde(default)]
    health_timeout_secs: u64,
    /// tar：已換上新的 unit（回滾時一併還原）
    unit_replaced: bool,
    /// 附在結果 `detail` 的說明（例如 `unit_not_updated`）
    notes: Vec<String>,
    /// 回滾的原因（健康確認沒有通過的項目）：回滾中重新開機後接續時沿用，結果的 `detail` 才不會只剩 `resumed`
    #[serde(default)]
    rollback_reason: String,
    /// 替換前實際執行的版本（輔助程式自己的版本，第一次執行時記下；0.10.0）。`request.json` 是服務帳號
    /// 寫的、不可信：回滾檔的版本、回滾後的健康確認與結果都以這裡為準，重新開機後接續時（執行檔已是新版）
    /// 也是。0.9.0 的狀態檔沒有這個欄位（空字串）
    #[serde(default)]
    from_version: String,
    /// 替換時使用的安裝方式（輔助程式自己偵測的；0.10.0）、目標與驗證過的回滾檔名：接續時以這裡為準，
    /// 不再採用 `request.json` 的值。0.9.0 的狀態檔沒有這些欄位（`method` 為空字串）
    #[serde(default)]
    method: String,
    #[serde(default)]
    target: Option<kairos_proto::mgmt::UpgradeTarget>,
    #[serde(default)]
    rollback_artifact: Option<String>,
    /// 這次升級已經把替換前的執行檔保留為 `pulse.prev`（之後不再覆寫，見 [`Helper::replace`]）
    #[serde(default)]
    prev_saved: bool,
    /// 最後的結果（`Done` 時）
    result: Option<UpgradeResult>,
}

/// `request.json` 的 `from_version` 與輔助程式自己認定的不符時，結果 `detail` 的開頭
pub const DETAIL_FROM_VERSION_MISMATCH: &str = "from_version_mismatch";
/// 接續時 `request.json` 的 `from_version` 與狀態檔記下的不同：沿用狀態檔的版本，結果 `detail` 加註
pub const NOTE_FROM_VERSION_IGNORED: &str = "request_from_version_ignored";
/// `request.json` 的 `method` 與輔助程式自己偵測到的安裝方式不符時，結果 `detail` 的開頭
pub const DETAIL_METHOD_MISMATCH: &str = "method_mismatch";
/// 狀態檔損毀或遺失、而這次升級已經開始替換時，結果 `detail` 的開頭
pub const DETAIL_STATE_UNUSABLE: &str = "state_unusable";
/// 替換前無法寫入狀態檔時，結果 `detail` 的開頭
pub const DETAIL_STATE_NOT_SAVED: &str = "state_not_saved";
/// 接續時 `request.json` 的 `method`、`target` 或 `rollback_artifact` 與狀態檔記下的不同：沿用狀態檔的內容
pub const NOTE_REQUEST_FIELDS_IGNORED: &str = "request_fields_ignored";

/// 日誌：stderr（journal）與工作目錄的 `upgrade.log`
struct Log {
    file: Option<std::fs::File>,
}

impl Log {
    fn new(path: Option<&Path>) -> Self {
        let file = path.and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        Self { file }
    }

    fn line(&mut self, level: &str, msg: &str) {
        let line = format!("{} [{level}] {msg}", crate::protocol::now_ts());
        eprintln!("{line}");
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// 輔助程式的結果（`state` 為終止狀態）
fn result(req: &Request, state: &str, code: Option<&str>, detail: String) -> UpgradeResult {
    UpgradeResult {
        schema: files::RESULT_SCHEMA.to_string(),
        upgrade_id: req.upgrade_id.clone(),
        state: state.to_string(),
        code: code.map(str::to_string),
        detail: super::truncate_detail(&detail),
        from_version: req.from_version.clone(),
        to_version: req.target.version.clone(),
        finished_at: crate::protocol::now_ts(),
    }
}

/// `pulse upgrade apply` 的進入點：回傳結束代碼（0 成功或已處理；1 無法處理請求）
pub fn run(request: &Path) -> u8 {
    let paths = Paths::system(request.to_path_buf());
    let sys = RealSystem::default();
    match apply(&paths, &sys) {
        Ok(r) => {
            if r.state == UPGRADE_STATE_ROLLBACK_FAILED {
                eprintln!(
                    "錯誤 {}：升級回滾失敗，需要人工處理（{}）",
                    codes::E_UPGRADE_ROLLBACK_FAILED,
                    r.detail
                );
            }
            0
        }
        Err(e) => {
            eprintln!("錯誤：無法處理遠端升級的請求：{e}");
            1
        }
    }
}

/// 執行一次請求（或接續上次中斷的請求）。無法讀取請求時回傳 `Err`（刪除 `request.json`，避免
/// `pulse-upgrade.path` 反覆觸發）
pub fn apply(paths: &Paths, sys: &dyn System) -> Result<UpgradeResult, String> {
    let req = match Request::read_untrusted(&paths.request) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&paths.request);
            return Err(e);
        }
    };
    let work = paths.work_root.join(&req.upgrade_id);
    crate::remote::create_dir_all_mode(&paths.work_root, 0o700)
        .and_then(|()| crate::remote::create_dir_all_mode(&work, 0o700))
        .map_err(|e| format!("無法建立 {}：{e}", work.display()))?;
    let mut log = Log::new(Some(&work.join("upgrade.log")));
    log.line(
        "INFO",
        &format!(
            "pulse upgrade apply：升級 {}（{} → {}，{}）",
            req.upgrade_id, req.from_version, req.target.version, req.method
        ),
    );
    let state_path = work.join("state.json");
    let state = read_state(&state_path, &work);
    let mut h = Helper {
        paths,
        sys,
        req,
        work,
        state_path,
        log,
    };
    let r = match state {
        Ok(state) => h.drive(state),
        // 狀態檔不可用而這次升級已經動過手：不當成全新的請求（那樣會改信 request.json，並可能以新版覆寫
        // pulse.prev）。不替換也不回滾，回報後由人處理
        Err(why) => h.fail(
            codes::E_UPGRADE_INTEGRITY,
            format!("{DETAIL_STATE_UNUSABLE}: {why}; manual check required"),
        ),
    };
    h.finish(&r);
    Ok(r)
}

/// 讀取這次升級的狀態檔。`Ok(None)`：全新的請求（沒有狀態檔，工作目錄中也沒有替換階段才會出現的檔案）。
/// `Err`：狀態檔存在但讀不到、不是合法的內容或 schema 不符，或狀態檔不見了而工作目錄裡已有替換階段的
/// 檔案（`rollback.pkg`、`pulse.service.prev`——都在第一次寫入狀態檔之後才建立）
fn read_state(state_path: &Path, work: &Path) -> Result<Option<HelperState>, String> {
    match std::fs::read(state_path) {
        Ok(b) => match serde_json::from_slice::<HelperState>(&b) {
            Ok(s) if s.schema == HELPER_SCHEMA => Ok(Some(s)),
            Ok(s) => Err(format!("state.json has schema {:?}", s.schema)),
            Err(e) => Err(format!("state.json is not valid: {e}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match ["rollback.pkg", "pulse.service.prev"]
                .iter()
                .find(|n| work.join(n).exists())
            {
                Some(n) => Err(format!("state.json is missing but {n} exists")),
                None => Ok(None),
            }
        }
        Err(e) => Err(format!("state.json cannot be read: {e}")),
    }
}

struct Helper<'a> {
    paths: &'a Paths,
    sys: &'a dyn System,
    req: Request,
    work: PathBuf,
    state_path: PathBuf,
    log: Log,
}

/// 預檢與替換需要的資料
struct Prepared {
    artifact: PathBuf,
    manifest: Option<ReleaseManifest>,
    rollback: Option<PathBuf>,
    new_binary: PathBuf,
    new_unit: Option<PathBuf>,
    health_timeout: Duration,
}

impl Helper<'_> {
    /// 寫入狀態檔。替換之後的步驟寫不進去時只能記下來繼續（不能半途停下；這次執行仍會做完健康確認與
    /// 回滾，只有重新開機後的接續受影響）
    fn save(&mut self, s: &HelperState) {
        if let Err(e) = self.try_save(s) {
            self.log.line("WARN", &e);
        }
    }

    /// 寫入狀態檔；失敗時回錯誤（替換前：不得繼續，見 [`Helper::replace`]）
    fn try_save(&mut self, s: &HelperState) -> Result<(), String> {
        let b = serde_json::to_vec_pretty(s).map_err(|e| format!("無法產生狀態檔：{e}"))?;
        crate::remote::write_atomic_durable(&self.state_path, &b, 0o600, 0o700)
            .map_err(|e| format!("無法寫入 {}：{e}", self.state_path.display()))
    }

    /// `request.json` 由服務帳號寫入、不可信（REMOTE.md 第 5.1、17.8 節）。決定「怎麼替換、回滾到哪一版」
    /// 的欄位以輔助程式自己可信的來源為準，請求中的值只用來比對：
    ///
    /// - **尚未替換**（第一次執行）：`from_version` 必須等於輔助程式自己的版本（＝目前安裝的版本），
    ///   `method` 必須等於輔助程式自己偵測到的安裝方式；不符就拒絕（`Err`，什麼都不改）。否則取得服務
    ///   帳號的人可以謊報較舊的 `from_version`、附上該舊版有官方簽章的套件當回滾檔，再讓新版的健康確認
    ///   失敗，使 root 以 `dpkg -i`／`rpm -U --oldpackage` 裝回任一個舊版，繞過本機的
    ///   `allow_downgrade = false`。
    /// - **已替換**（重新開機後接續；執行檔已是新版）：`from_version`、`method`、`target`、
    ///   `rollback_artifact` 一律取自替換前寫進狀態檔（只有 root 可寫）的值，不再採用請求中的值；請求
    ///   被改過時在結果加註（不能半途停下）。
    ///
    /// 之後 `self.req` 的這些欄位都是可信的值。0.9.0 的輔助程式寫的狀態檔沒有記這些欄位：接續時只能
    /// 沿用請求中的值（那一次升級由 0.9.0 的輔助程式開始，行為與 0.9.0 相同）
    fn trust_request(&mut self, state: Option<&mut HelperState>) -> Result<(), Box<UpgradeResult>> {
        let replaced = state
            .as_ref()
            .is_some_and(|s| matches!(s.step, Step::Replaced | Step::RollingBack));
        let recorded_from = state
            .as_ref()
            .map(|s| s.from_version.clone())
            .filter(|v| !v.is_empty());
        if replaced {
            let Some(s) = state else {
                return Ok(());
            };
            let Some(from) = recorded_from else {
                self.log.line(
                    "WARN",
                    "狀態檔沒有記錄替換前的版本與安裝方式（由 0.9.0 的輔助程式寫入），沿用請求中的值",
                );
                return Ok(());
            };
            if self.req.from_version != from {
                self.log.line(
                    "WARN",
                    &format!(
                        "request.json 的 from_version（{}）與替換前記下的版本（{from}）不同，以後者為準",
                        self.req.from_version
                    ),
                );
                self.req.from_version = from;
                if !s.notes.iter().any(|n| n == NOTE_FROM_VERSION_IGNORED) {
                    s.notes.push(NOTE_FROM_VERSION_IGNORED.to_string());
                }
            }
            let mut changed = Vec::new();
            if !s.method.is_empty() && self.req.method != s.method {
                changed.push(format!("method {} → {}", self.req.method, s.method));
                self.req.method = s.method.clone();
            }
            if let Some(t) = &s.target
                && self.req.target != *t
            {
                changed.push(format!(
                    "target {} → {}",
                    self.req.target.version, t.version
                ));
                self.req.target = t.clone();
            }
            if !s.method.is_empty() && self.req.rollback_artifact != s.rollback_artifact {
                changed.push("rollback_artifact".to_string());
                self.req.rollback_artifact = s.rollback_artifact.clone();
            }
            if !changed.is_empty() {
                self.log.line(
                    "WARN",
                    &format!(
                        "request.json 在替換後被改過（{}），以替換前記下的內容為準",
                        changed.join("、")
                    ),
                );
                if !s.notes.iter().any(|n| n == NOTE_REQUEST_FIELDS_IGNORED) {
                    s.notes.push(NOTE_REQUEST_FIELDS_IGNORED.to_string());
                }
            }
            return Ok(());
        }
        let claimed = self.req.from_version.clone();
        let trusted = recorded_from.unwrap_or_else(|| self.paths.current_version.clone());
        if claimed != trusted {
            self.req.from_version = trusted.clone();
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_INTEGRITY,
                format!(
                    "{DETAIL_FROM_VERSION_MISMATCH}: request says {claimed}, installed version is {trusted}"
                ),
            )));
        }
        if self.req.method != self.paths.method {
            let detail = format!(
                "{DETAIL_METHOD_MISMATCH}: request says {}, detected install method is {}",
                self.req.method, self.paths.method
            );
            return Err(Box::new(self.fail(codes::E_UPGRADE_INTEGRITY, detail)));
        }
        Ok(())
    }

    fn drive(&mut self, mut state: Option<HelperState>) -> UpgradeResult {
        if let Some(HelperState {
            step: Step::Done,
            result: Some(r),
            ..
        }) = &state
        {
            return r.clone();
        }
        if let Err(r) = self.trust_request(state.as_mut()) {
            return *r;
        }
        let mut prior = None;
        if let Some(s) = state {
            match s.step {
                Step::Done => {}
                Step::Replaced => {
                    self.log
                        .line("INFO", "接續上次中斷的升級：已替換，重新做健康確認");
                    return self.after_replace(s);
                }
                Step::RollingBack => {
                    self.log.line("INFO", "接續上次中斷的回滾");
                    let reason = if s.rollback_reason.is_empty() {
                        "resumed".to_string()
                    } else {
                        format!("{}; resumed", s.rollback_reason)
                    };
                    return self.confirm_rollback(s, reason);
                }
                // 上一次執行在「寫入狀態檔」與「記下已替換」之間中斷：重新驗證後再替換一次，但沿用當時
                // 記下的內容（pulse.prev 是否已保留）
                Step::Verified => prior = Some(s),
            }
        }
        let prepared = match self.prepare() {
            Ok(p) => p,
            Err(r) => return *r,
        };
        self.replace(prepared, prior)
    }

    fn fail(&mut self, code: &str, detail: String) -> UpgradeResult {
        self.log
            .line("ERROR", &format!("升級失敗（{code}）：{detail}"));
        result(&self.req, UPGRADE_STATE_FAILED, Some(code), detail)
    }

    fn staged(&self, name: &str) -> PathBuf {
        self.paths
            .upgrade_dir()
            .join(&self.req.upgrade_id)
            .join(name)
    }

    /// 步驟 1～3：複製、重新驗證、預檢
    fn prepare(&mut self) -> Result<Prepared, Box<UpgradeResult>> {
        let req = self.req.clone();
        // 1. 複製
        let copy = |this: &mut Self, name: &str, max: u64| -> Result<PathBuf, Box<UpgradeResult>> {
            let dst = this.work.join(name);
            match files::copy_untrusted(&this.staged(name), &dst, max) {
                Ok(_) => Ok(dst),
                Err(e) => Err(Box::new(this.fail(codes::E_UPGRADE_INTEGRITY, e))),
            }
        };
        let artifact = copy(self, &req.artifact, files::MAX_FILE_BYTES)?;
        let sums_path = copy(self, files::SUMS_FILE, MAX_SUMS)?;
        let sig_path = copy(self, files::SIG_FILE, MAX_SIG)?;
        let manifest_path = match &req.manifest {
            Some(m) => Some(copy(self, m, MAX_MANIFEST)?),
            None => None,
        };
        let mut rollback = None;
        let mut rollback_sums = None;
        if let Some(r) = &req.rollback_artifact {
            match (
                copy(self, r, files::MAX_FILE_BYTES),
                copy(self, files::ROLLBACK_SUMS_FILE, MAX_SUMS),
                copy(self, files::ROLLBACK_SIG_FILE, MAX_SIG),
            ) {
                (Ok(a), Ok(s), Ok(g)) => {
                    rollback = Some(a);
                    rollback_sums = Some((s, g));
                }
                _ => self.log.line(
                    "WARN",
                    "回滾檔或它的簽章資料無法使用，回滾時改以 pulse.prev 回復",
                ),
            }
        }
        // 2. 設定與信任錨（只讀 pulse.toml）
        let config = match AppConfig::load_file_only(&self.paths.install_dir.join("pulse.toml")) {
            Ok(c) => c,
            Err(e) => {
                return Err(Box::new(self.fail(
                    codes::E_UPGRADE_PRECHECK,
                    format!("pulse.toml 無法載入：{e}"),
                )));
            }
        };
        if !config.upgrade.enabled {
            let r = result(
                &req,
                UPGRADE_STATE_FAILED,
                Some(codes::I_UPGRADE_DISABLED),
                "remote upgrade is disabled in pulse.toml".to_string(),
            );
            self.log
                .line("WARN", "pulse.toml 的 upgrade.enabled = false，不替換");
            return Err(Box::new(r));
        }
        let anchor = {
            let p = config.upgrade.allowed_signers_path();
            if p.is_relative() {
                self.paths.install_dir.join(p)
            } else {
                p
            }
        };
        let signers =
            match crate::remote::install::load_trust_anchor(&anchor, self.paths.require_root_owner)
            {
                Ok(s) => s,
                Err(e) => return Err(Box::new(self.fail(codes::E_UPGRADE_TRUST_ANCHOR, e))),
            };
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        let sums = read(&sums_path);
        let sig = read(&sig_path);
        let manifest_text = manifest_path.as_deref().map(read);
        let trust = TrustPolicy {
            signers: &signers,
            extra_principals: &config.upgrade.extra_principals,
            now_unix: self.sys.now_unix(),
        };
        let bundle = ReleaseBundle {
            sha256sums: &sums,
            signature: &sig,
            manifest: req.manifest.as_deref().zip(manifest_text.as_deref()),
        };
        let verified = match release::verify_bundle(&bundle, &trust) {
            Ok(v) => v,
            Err(e) => {
                let code = if e.code_number() == 8401 {
                    codes::E_UPGRADE_SIGNATURE
                } else {
                    codes::E_UPGRADE_INTEGRITY
                };
                return Err(Box::new(self.fail(code, e.to_string())));
            }
        };
        let format = super::method_format(&req.method);
        let arch = Arch::parse(&self.paths.arch);
        let expect = ArtifactExpectation {
            product: Some(Product::Pulse),
            version: Some(&req.target.version),
            commit: Some(&req.target.commit),
            os: Some(Os::Linux),
            arch,
            format,
        };
        let actual = files::sha256_file(&artifact).unwrap_or_default();
        if format.is_none() || arch.is_none() {
            return Err(Box::new(
                self.fail(codes::W_UPGRADE_UNSUPPORTED, "unsupported_platform".into()),
            ));
        }
        if let Err(e) = verified.check_artifact(&req.artifact, &actual, &expect) {
            return Err(Box::new(
                self.fail(codes::E_UPGRADE_INTEGRITY, e.to_string()),
            ));
        }
        let (Ok(current), Ok(target)) = (
            Version::parse(&self.paths.current_version),
            Version::parse(&req.target.version),
        ) else {
            return Err(Box::new(
                self.fail(codes::E_UPGRADE_INTEGRITY, "invalid_version".into()),
            ));
        };
        if let Err(p) = release::check_version_policy(
            &current,
            &target,
            verified.manifest.as_ref(),
            req.allow_downgrade,
            config.upgrade.allow_downgrade,
        ) {
            let code = match p {
                PolicyDecline::DowngradeNotAllowed => codes::W_UPGRADE_DOWNGRADE,
                _ => codes::W_UPGRADE_UNSUPPORTED,
            };
            return Err(Box::new(self.fail(code, p.detail().to_string())));
        }
        // 回滾檔：以它所屬發佈的 SHA256SUMS（本機驗證過的紀錄）重新驗證
        if let (Some(r), Some((s, g))) = (&rollback, &rollback_sums) {
            let rs = read(s);
            let rg = read(g);
            let ok = release::verify_bundle(
                &ReleaseBundle {
                    sha256sums: &rs,
                    signature: &rg,
                    manifest: None,
                },
                &trust,
            )
            .ok()
            .and_then(|v| {
                let name = req.rollback_artifact.clone().unwrap_or_default();
                // 版本以輔助程式自己認定的「替換前的版本」為準（trust_request），不是請求中的值
                let e = ArtifactExpectation {
                    product: Some(Product::Pulse),
                    version: Some(&req.from_version),
                    commit: None,
                    os: Some(Os::Linux),
                    arch,
                    format,
                };
                v.check_artifact(&name, &files::sha256_file(r).unwrap_or_default(), &e)
                    .ok()
            })
            .is_some();
            if !ok {
                self.log
                    .line("WARN", "回滾檔沒有通過驗證，回滾時改以 pulse.prev 回復");
                rollback = None;
            }
        }
        self.log.line(
            "INFO",
            &format!(
                "已重新驗證 {}（簽署者 {}）",
                req.artifact, verified.signer.fingerprint
            ),
        );
        // 3. 預檢
        let xdir = self
            .paths
            .install_dir
            .join(format!(".upgrade-{}", req.upgrade_id));
        let _ = std::fs::remove_dir_all(&xdir);
        if let Err(e) = crate::remote::create_dir_all_mode(&xdir, 0o755) {
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_PRECHECK,
                format!("無法建立 {}：{e}", xdir.display()),
            )));
        }
        let cmd = match req.method.as_str() {
            "tar" => Cmd::TarExtract {
                archive: artifact.clone(),
                dir: xdir.clone(),
            },
            "deb" => Cmd::DebExtract {
                package: artifact.clone(),
                dir: xdir.clone(),
            },
            _ => Cmd::RpmExtract {
                package: artifact.clone(),
                dir: xdir.clone(),
            },
        };
        let out = self.sys.run(&cmd);
        if !out.ok() {
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_PRECHECK,
                format!("無法解開發佈檔：{}", out.last_line()),
            )));
        }
        let (new_binary, new_unit) = match locate_binary(&req.method, &xdir) {
            Ok(x) => x,
            Err(e) => return Err(Box::new(self.fail(codes::E_UPGRADE_PRECHECK, e))),
        };
        let version_out = self.sys.run(&Cmd::RunAsService {
            program: new_binary.clone(),
            args: vec!["--version".into()],
            cwd: self.paths.install_dir.clone(),
            state_dir: self.paths.state_dir(),
        });
        let reported = version_out.stdout.split_whitespace().nth(1).unwrap_or("");
        if !version_out.ok() || reported != req.target.version {
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_PRECHECK,
                format!(
                    "新版的 --version 不符（預期 {}，實際 {:?}，{}）",
                    req.target.version,
                    version_out.stdout.trim(),
                    version_out.last_line()
                ),
            )));
        }
        let check = self.sys.run(&Cmd::RunAsService {
            program: new_binary.clone(),
            args: vec![
                "check-config".into(),
                "--json".into(),
                "--with-remote".into(),
            ],
            cwd: self.paths.install_dir.clone(),
            state_dir: self.paths.state_dir(),
        });
        if !matches!(check.status, Some(0 | 8)) {
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_PRECHECK,
                format!(
                    "目前的設定無法通過新版的檢查（check-config 結束代碼 {:?}）",
                    check.status
                ),
            )));
        }
        self.log.line("INFO", "預檢通過（版本與設定檢查）");
        let manifest = verified.manifest.clone();
        let health_timeout = Duration::from_secs(u64::from(
            kairos_proto::mgmt::effective_health_timeout_secs(
                req.health_timeout_secs,
                config.upgrade.health_timeout_secs,
            ),
        ));
        Ok(Prepared {
            artifact,
            manifest,
            rollback,
            new_binary,
            new_unit,
            health_timeout,
        })
    }

    /// 步驟 4：保留舊版、替換、重新啟動。
    ///
    /// 順序（fail closed）：**先寫入狀態檔**（`Verified`：替換前的版本、安裝方式、目標）——接續時只信這份
    /// 狀態檔，寫不進去就不得開始替換；再保留 `pulse.prev` 與回滾檔，並把「已保留」記進狀態檔（同樣寫不
    /// 進去就停）。`prior` 是上一次執行留下的 `Verified` 狀態：當時已保留過 `pulse.prev` 的話不再覆寫——
    /// 上一次可能在替換完成、還沒記下 `Replaced` 時中斷，此時目前的執行檔已是新版，再複製一次會把舊版的
    /// 備份蓋掉，之後的回滾就換不回舊版
    fn replace(&mut self, p: Prepared, prior: Option<HelperState>) -> UpgradeResult {
        let install = self.paths.install_dir.clone();
        let current = install.join("pulse");
        let prev = install.join("pulse.prev");
        let prev_saved = prior.as_ref().is_some_and(|s| s.prev_saved) && prev.is_file();
        let mut state = HelperState {
            schema: HELPER_SCHEMA.to_string(),
            step: Step::Verified,
            deadline_unix: 0,
            n_restarts_before: 0,
            restarted_unix: 0,
            health_timeout_secs: p.health_timeout.as_secs(),
            unit_replaced: false,
            notes: Vec::new(),
            rollback_reason: String::new(),
            // 已在 trust_request 確認與輔助程式自己的版本、偵測到的安裝方式相同
            from_version: self.req.from_version.clone(),
            method: self.req.method.clone(),
            target: Some(self.req.target.clone()),
            // 只記通過驗證的回滾檔（之後複製到工作目錄的 rollback.pkg）
            rollback_artifact: p.rollback.as_ref().and(self.req.rollback_artifact.clone()),
            prev_saved,
            result: None,
        };
        if let Err(e) = self.try_save(&state) {
            return self.fail(
                codes::E_UPGRADE_REPLACE,
                format!("{DETAIL_STATE_NOT_SAVED}: {e}"),
            );
        }
        if prev_saved {
            self.log.line(
                "INFO",
                "上一次執行已保留 pulse.prev（在記下「已替換」之前中斷），不再覆寫",
            );
        } else {
            if let Err(e) = copy_executable(&current, &prev) {
                return self.fail(
                    codes::E_UPGRADE_REPLACE,
                    format!("無法保留目前的執行檔：{e}"),
                );
            }
            state.prev_saved = true;
            if let Err(e) = self.try_save(&state) {
                return self.fail(
                    codes::E_UPGRADE_REPLACE,
                    format!("{DETAIL_STATE_NOT_SAVED}: {e}"),
                );
            }
        }
        // 回滾檔留在工作目錄（重新開機後接續時使用）
        if let Some(r) = &p.rollback {
            let _ = std::fs::copy(r, self.work.join("rollback.pkg"));
        }
        let before = self.sys.service_state().map(|s| s.n_restarts).unwrap_or(0);
        let replaced = match self.req.method.as_str() {
            "tar" => {
                let new = install.join("pulse.new");
                copy_executable(&p.new_binary, &new)
                    .and_then(|()| std::fs::rename(&new, &current))
                    .map_err(|e| format!("無法替換執行檔：{e}"))
            }
            "deb" => self.package(&Cmd::DpkgInstall {
                package: p.artifact.clone(),
            }),
            _ => self.package(&Cmd::RpmInstall {
                package: p.artifact.clone(),
            }),
        };
        if let Err(e) = replaced {
            // 替換前後的執行檔不同時換回
            if files::sha256_file(&current).ok() != files::sha256_file(&prev).ok() {
                let _ = restore_executable(&prev, &current);
            }
            return self.fail(codes::E_UPGRADE_REPLACE, e);
        }
        if self.req.method == "tar" {
            self.maybe_replace_unit(&p, &mut state);
        }
        let _ = std::fs::remove_dir_all(install.join(format!(".upgrade-{}", self.req.upgrade_id)));
        self.log.line(
            "INFO",
            &format!(
                "已替換為 {}，重新啟動 pulse.service",
                self.req.target.version
            ),
        );
        state.step = Step::Replaced;
        state.n_restarts_before = before;
        state.restarted_unix = self.sys.now_unix();
        state.deadline_unix =
            self.sys.now_unix() + i64::try_from(p.health_timeout.as_secs()).unwrap_or(i64::MAX);
        self.save(&state);
        if let Err(e) = self.sys.restart_service() {
            self.log
                .line("WARN", &format!("systemctl restart 失敗：{e}"));
        }
        self.after_replace(state)
    }

    /// 套件管理員（被鎖住時每 30 秒重試，最多 10 分鐘）
    fn package(&mut self, cmd: &Cmd) -> Result<(), String> {
        let start = self.sys.mono();
        loop {
            let out = self.sys.run(cmd);
            if out.ok() {
                return Ok(());
            }
            let locked = out.stderr.to_ascii_lowercase().contains("lock");
            if !locked || self.sys.mono().saturating_sub(start) >= LOCK_GIVE_UP {
                return Err(out.last_line());
            }
            self.log.line(
                "INFO",
                &format!(
                    "套件資料庫被鎖住，{} 秒後重試：{}",
                    LOCK_RETRY.as_secs(),
                    out.last_line()
                ),
            );
            self.sys.sleep(LOCK_RETRY);
        }
    }

    /// tar：發佈清單的 unit 雜湊與目前的 unit 不同、而目前的 unit 是歷來官方版本之一（沒被改過）時換新
    fn maybe_replace_unit(&mut self, p: &Prepared, state: &mut HelperState) {
        let (Some(m), Some(new_unit)) = (&p.manifest, &p.new_unit) else {
            return;
        };
        let Some(u) = &m.unit else {
            return;
        };
        let current = files::sha256_file(&self.paths.unit_path).ok();
        match current {
            Some(c) if c == u.sha256 => {}
            Some(c) if u.previous_sha256.contains(&c) => {
                let backup = self.work.join("pulse.service.prev");
                let ok = std::fs::copy(&self.paths.unit_path, &backup)
                    .and_then(|_| std::fs::copy(new_unit, &self.paths.unit_path))
                    .is_ok();
                if ok {
                    state.unit_replaced = true;
                    if let Err(e) = self.sys.daemon_reload() {
                        self.log.line("WARN", &format!("daemon-reload 失敗：{e}"));
                    }
                    self.log
                        .line("INFO", "已換上新版的 pulse.service（沒有被修改過）");
                }
            }
            _ => {
                state.notes.push("unit_not_updated".to_string());
                self.log.line(
                    "WARN",
                    &format!(
                        "{} 被修改過或不存在，沒有換上新版的 unit（{}）",
                        self.paths.unit_path.display(),
                        codes::W_UPGRADE_UNSUPPORTED
                    ),
                );
            }
        }
    }

    /// 步驟 5：健康確認；失敗則回滾
    fn after_replace(&mut self, mut state: HelperState) -> UpgradeResult {
        let now = self.sys.now_unix();
        let remaining = Duration::from_secs(u64::try_from(state.deadline_unix - now).unwrap_or(0));
        let target = self.req.target.version.clone();
        let check = if remaining.is_zero() {
            Err("health_timeout".to_string())
        } else {
            self.health(&target, &state, remaining)
        };
        match check {
            Ok(()) => {
                let mut detail = String::new();
                if !state.notes.is_empty() {
                    detail = state.notes.join(",");
                }
                self.log.line(
                    "INFO",
                    &format!("健康確認通過，已升級到 {}", self.req.target.version),
                );
                let r = result(
                    &self.req,
                    UPGRADE_STATE_SUCCEEDED,
                    Some(codes::I_UPGRADE_COMPLETED),
                    detail,
                );
                state.step = Step::Done;
                state.result = Some(r.clone());
                self.save(&state);
                r
            }
            Err(reason) => {
                self.log.line(
                    "WARN",
                    &format!("健康確認失敗（{reason}），回滾到 {}", self.req.from_version),
                );
                self.rollback(state, reason)
            }
        }
    }

    /// 步驟 6：回滾
    fn rollback(&mut self, mut state: HelperState, reason: String) -> UpgradeResult {
        let install = self.paths.install_dir.clone();
        let current = install.join("pulse");
        let prev = install.join("pulse.prev");
        let rollback_pkg = self.work.join("rollback.pkg");
        let restored: Result<(), String> = match self.req.method.as_str() {
            "deb" | "rpm" if rollback_pkg.is_file() => {
                self.log.line(
                    "INFO",
                    &format!(
                        "以回滾檔 {} 經套件管理員裝回 {}（{}）",
                        self.req
                            .rollback_artifact
                            .as_deref()
                            .unwrap_or("rollback.pkg"),
                        self.req.from_version,
                        if self.req.method == "deb" {
                            "dpkg -i"
                        } else {
                            "rpm -U --oldpackage --replacepkgs"
                        }
                    ),
                );
                let cmd = if self.req.method == "deb" {
                    Cmd::DpkgInstall {
                        package: rollback_pkg.clone(),
                    }
                } else {
                    Cmd::RpmInstall {
                        package: rollback_pkg.clone(),
                    }
                };
                self.package(&cmd)
            }
            m => {
                if m != "tar" {
                    state.notes.push(format!(
                        "binary_restored_package_db_shows_{}",
                        self.req.target.version
                    ));
                }
                self.log.line(
                    "INFO",
                    &format!(
                        "以替換前保留的 {} 換回 {}{}",
                        prev.display(),
                        self.req.from_version,
                        if m == "tar" {
                            ""
                        } else {
                            "（沒有可驗證的回滾檔，套件資料庫仍顯示新版）"
                        }
                    ),
                );
                let r = restore_executable(&prev, &current)
                    .map_err(|e| format!("無法換回 pulse.prev：{e}"));
                if r.is_ok() && state.unit_replaced {
                    let backup = self.work.join("pulse.service.prev");
                    if std::fs::copy(&backup, &self.paths.unit_path).is_ok() {
                        let _ = self.sys.daemon_reload();
                    }
                }
                r
            }
        };
        if let Err(e) = restored {
            return self.rollback_failed(state, format!("{reason}; rollback: {e}"));
        }
        state.step = Step::RollingBack;
        state.rollback_reason = reason.clone();
        state.n_restarts_before = self.sys.service_state().map(|s| s.n_restarts).unwrap_or(0);
        state.restarted_unix = self.sys.now_unix();
        let timeout = Duration::from_secs(state.health_timeout_secs).max(MIN_ROLLBACK_TIMEOUT);
        state.deadline_unix =
            state.restarted_unix + i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX);
        self.save(&state);
        if let Err(e) = self.sys.restart_service() {
            self.log
                .line("WARN", &format!("systemctl restart 失敗：{e}"));
        }
        self.confirm_rollback(state, reason)
    }

    fn confirm_rollback(&mut self, mut state: HelperState, reason: String) -> UpgradeResult {
        let remaining = Duration::from_secs(
            u64::try_from(state.deadline_unix - self.sys.now_unix()).unwrap_or(0),
        );
        let from = self.req.from_version.clone();
        match self.health(&from, &state, remaining) {
            Ok(()) => {
                let mut detail = reason;
                if !state.notes.is_empty() {
                    detail = format!("{detail}; {}", state.notes.join(","));
                }
                self.log
                    .line("WARN", &format!("已回滾到 {from}（{detail}）"));
                let r = result(
                    &self.req,
                    UPGRADE_STATE_ROLLED_BACK,
                    Some(codes::W_UPGRADE_ROLLED_BACK),
                    detail,
                );
                state.step = Step::Done;
                state.result = Some(r.clone());
                self.save(&state);
                r
            }
            Err(e) => self.rollback_failed(state, format!("{reason}; after rollback: {e}")),
        }
    }

    fn rollback_failed(&mut self, mut state: HelperState, detail: String) -> UpgradeResult {
        self.log.line(
            "ERROR",
            &format!(
                "{}：回滾失敗，需要人工處理（{detail}）；暫存與紀錄保留在 {}",
                codes::E_UPGRADE_ROLLBACK_FAILED,
                self.work.display()
            ),
        );
        let r = result(
            &self.req,
            UPGRADE_STATE_ROLLBACK_FAILED,
            Some(codes::E_UPGRADE_ROLLBACK_FAILED),
            detail,
        );
        state.step = Step::Done;
        state.result = Some(r.clone());
        self.save(&state);
        r
    }

    /// 健康確認（REMOTE.md 第 4.5.1 節）
    fn health(
        &mut self,
        version: &str,
        state: &HelperState,
        timeout: Duration,
    ) -> Result<(), String> {
        let start = self.sys.mono();
        let status_path = self.status_path();
        let mut stable_since: Option<Duration> = None;
        let mut last = "not_started".to_string();
        loop {
            let elapsed = self.sys.mono().saturating_sub(start);
            match self.sys.service_state() {
                Ok(s) => {
                    if s.n_restarts > state.n_restarts_before {
                        return Err(format!(
                            "crash_restart (NRestarts {} → {})",
                            state.n_restarts_before, s.n_restarts
                        ));
                    }
                    if matches!(s.active.as_str(), "failed" | "inactive")
                        && let Some(code @ 2..=4) = s.exit_status
                    {
                        return Err(format!("exit_code_{code}"));
                    }
                    let healthy = if s.active != "active" {
                        last = format!("service_{}", s.active);
                        false
                    } else {
                        match crate::status_file::read(&status_path) {
                            Ok(f) => {
                                let acked_after = f
                                    .reporting
                                    .last_ack_at
                                    .as_deref()
                                    .and_then(crate::status_file::parse_ts)
                                    .is_some_and(|t| t.timestamp() >= state.restarted_unix);
                                if f.pid != s.main_pid || f.version != version || !f.running {
                                    last = "status_not_from_new_process".into();
                                    false
                                } else if f.connection.state != "registered" {
                                    last = format!("not_registered ({})", f.connection.state);
                                    false
                                } else if !acked_after {
                                    last = "no_report_acked".into();
                                    false
                                } else {
                                    true
                                }
                            }
                            Err(e) => {
                                last = format!("status_file_unreadable ({e:?})");
                                false
                            }
                        }
                    };
                    if healthy {
                        let since = *stable_since.get_or_insert(elapsed);
                        if elapsed.saturating_sub(since) >= STABLE_FOR {
                            return Ok(());
                        }
                    } else {
                        stable_since = None;
                    }
                }
                Err(e) => {
                    last = format!("systemctl_show_failed ({e})");
                    stable_since = None;
                }
            }
            if elapsed >= timeout {
                return Err(format!("health_timeout ({last})"));
            }
            self.sys.sleep(POLL);
        }
    }

    /// Pulse 的狀態檔：`pulse.toml` 的 `status.file`（相對路徑以安裝目錄解析）或狀態目錄的 `status.json`
    fn status_path(&self) -> PathBuf {
        let configured = AppConfig::load_file_only(&self.paths.install_dir.join("pulse.toml"))
            .map(|c| c.status.file.trim().to_string())
            .unwrap_or_default();
        if configured.is_empty() {
            self.paths.state_dir().join("status.json")
        } else {
            let p = PathBuf::from(configured);
            if p.is_relative() {
                self.paths.install_dir.join(p)
            } else {
                p
            }
        }
    }

    /// 步驟 7：寫結果、刪除請求與暫存
    fn finish(&mut self, r: &UpgradeResult) {
        let bytes = serde_json::to_vec_pretty(r).unwrap_or_default();
        if let Err(e) = crate::remote::write_atomic(&self.paths.result_path(), &bytes, 0o644, 0o750)
        {
            self.log.line(
                "ERROR",
                &format!("無法寫入 {}：{e}", self.paths.result_path().display()),
            );
        }
        let _ = std::fs::remove_file(&self.paths.request);
        if r.state != UPGRADE_STATE_ROLLBACK_FAILED {
            let _ = std::fs::remove_dir_all(self.paths.upgrade_dir().join(&self.req.upgrade_id));
            let _ = std::fs::remove_file(self.work.join("rollback.pkg"));
            for name in std::fs::read_dir(&self.work)
                .into_iter()
                .flatten()
                .flatten()
            {
                let n = name.file_name().to_string_lossy().to_string();
                if n.starts_with("pulse-")
                    || n == files::SUMS_FILE
                    || n == files::SIG_FILE
                    || n == files::ROLLBACK_SUMS_FILE
                    || n == files::ROLLBACK_SIG_FILE
                {
                    let _ = std::fs::remove_file(name.path());
                }
            }
            // 只保留最近一次的紀錄
            if let Ok(entries) = std::fs::read_dir(&self.paths.work_root) {
                for e in entries.flatten() {
                    if e.path() != self.work {
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            }
        }
        self.log.line(
            "INFO",
            &format!("結果：{}（{}）", r.state, r.code.as_deref().unwrap_or("-")),
        );
    }
}

/// 解開的發佈檔中的新執行檔（與 tar 的 unit）
fn locate_binary(method: &str, xdir: &Path) -> Result<(PathBuf, Option<PathBuf>), String> {
    if method == "tar" {
        let dirs: Vec<PathBuf> = std::fs::read_dir(xdir)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        let [top] = dirs.as_slice() else {
            return Err("發佈檔的版面不符（預期只有一個最上層目錄）".to_string());
        };
        let bin = top.join("pulse");
        if !bin.is_file() {
            return Err("發佈檔中沒有 pulse 執行檔".to_string());
        }
        let unit = Some(top.join("pulse.service")).filter(|u| u.is_file());
        Ok((bin, unit))
    } else {
        let bin = xdir.join("opt/pulse/pulse");
        if !bin.is_file() {
            return Err("套件中沒有 /opt/pulse/pulse".to_string());
        }
        Ok((bin, None))
    }
}

/// 複製執行檔（0755），寫完 fsync
fn copy_executable(src: &Path, dst: &Path) -> std::io::Result<()> {
    let tmp = dst.with_extension("tmp-copy");
    let mut out = crate::remote::open_new_file(&tmp, 0o755)?;
    let mut input = std::fs::File::open(src)?;
    std::io::copy(&mut input, &mut out)?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&tmp, dst)
}

/// 以 `pulse.prev` 換回目前的執行檔（複製後改名，保留 `pulse.prev`）
fn restore_executable(prev: &Path, current: &Path) -> std::io::Result<()> {
    let tmp = current.with_extension("restore");
    copy_executable(prev, &tmp)?;
    std::fs::rename(&tmp, current)
}

/// 正式環境的 [`System`]
#[derive(Debug)]
pub struct RealSystem {
    start: std::time::Instant,
}

impl Default for RealSystem {
    fn default() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

fn first_existing(candidates: &[&'static str]) -> &'static str {
    candidates
        .iter()
        .find(|p| Path::new(p).is_file())
        .copied()
        .unwrap_or(candidates[0])
}

fn run_command(mut c: std::process::Command) -> Output {
    c.stdin(std::process::Stdio::null());
    match c.output() {
        Ok(o) => Output {
            status: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Output {
            status: None,
            stdout: String::new(),
            stderr: e.to_string(),
        },
    }
}

impl RealSystem {
    fn systemctl(&self, args: &[&str]) -> Output {
        let mut c =
            std::process::Command::new(first_existing(&["/usr/bin/systemctl", "/bin/systemctl"]));
        c.args(args);
        run_command(c)
    }
}

/// 服務帳號的 uid／gid
#[cfg(unix)]
fn service_ids() -> Option<(u32, u32)> {
    let name = std::ffi::CString::new(SERVICE_USER).ok()?;
    // SAFETY: getpwnam 回傳指向靜態區的指標（或 NULL）；只在這個執行緒立即讀取欄位
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    // SAFETY: 上面已確認不是 NULL
    let pw = unsafe { &*pw };
    Some((pw.pw_uid, pw.pw_gid))
}

impl System for RealSystem {
    fn mono(&self) -> Duration {
        self.start.elapsed()
    }

    fn now_unix(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn restart_service(&self) -> Result<(), String> {
        let o = self.systemctl(&["restart", "--no-block", "pulse.service"]);
        if o.ok() { Ok(()) } else { Err(o.last_line()) }
    }

    fn service_state(&self) -> Result<ServiceState, String> {
        let o = self.systemctl(&[
            "show",
            "pulse.service",
            "--property=ActiveState,NRestarts,MainPID,ExecMainStatus",
        ]);
        if !o.ok() {
            return Err(o.last_line());
        }
        Ok(parse_show(&o.stdout))
    }

    fn daemon_reload(&self) -> Result<(), String> {
        let o = self.systemctl(&["daemon-reload"]);
        if o.ok() { Ok(()) } else { Err(o.last_line()) }
    }

    fn run(&self, cmd: &Cmd) -> Output {
        use std::process::Command;
        match cmd {
            Cmd::TarExtract { archive, dir } => {
                let mut c = Command::new(first_existing(&["/usr/bin/tar", "/bin/tar"]));
                c.arg("-xzf")
                    .arg(archive)
                    .arg("-C")
                    .arg(dir)
                    .arg("--no-same-owner");
                run_command(c)
            }
            Cmd::DebExtract { package, dir } => {
                let mut c = Command::new(first_existing(&["/usr/bin/dpkg-deb", "/bin/dpkg-deb"]));
                c.arg("-x").arg(package).arg(dir);
                run_command(c)
            }
            Cmd::RpmExtract { package, dir } => rpm_extract(
                Path::new(first_existing(&["/usr/bin/rpm2cpio", "/bin/rpm2cpio"])),
                package,
                Path::new(first_existing(&["/usr/bin/cpio", "/bin/cpio"])),
                dir,
            ),
            Cmd::DpkgInstall { package } => {
                let mut c = Command::new(first_existing(&["/usr/bin/dpkg", "/bin/dpkg"]));
                c.env("DEBIAN_FRONTEND", "noninteractive")
                    .args(["-i", "--force-confold"])
                    .arg(package);
                run_command(c)
            }
            Cmd::RpmInstall { package } => run_command(rpm_install_command(
                Path::new(first_existing(&["/usr/bin/rpm", "/bin/rpm"])),
                package,
            )),
            Cmd::RunAsService {
                program,
                args,
                cwd,
                state_dir,
            } => {
                let mut c = Command::new(program);
                c.args(args)
                    .current_dir(cwd)
                    .env_clear()
                    .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                    .env("STATE_DIRECTORY", state_dir);
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    match service_ids() {
                        Some((uid, gid)) => {
                            c.uid(uid).gid(gid);
                        }
                        None => {
                            return Output {
                                status: None,
                                stdout: String::new(),
                                stderr: format!("找不到服務帳號 {SERVICE_USER}"),
                            };
                        }
                    }
                }
                run_command(c)
            }
        }
    }
}

/// `rpm -U --oldpackage --replacepkgs <套件>`（[`Cmd::RpmInstall`]）
fn rpm_install_command(rpm: &Path, package: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(rpm);
    c.args(["-U", "--oldpackage", "--replacepkgs"]).arg(package);
    c
}

/// `rpm2cpio <套件> | cpio -idm`（在 `dir` 中）：兩者都成功才算成功。
///
/// cpio 無法執行（例如沒有安裝：Rocky／RHEL 9 的最小安裝沒有 cpio）時，cpio 的 `Command` 仍握著管線的
/// 讀取端，若在它被釋放前等待 rpm2cpio，rpm2cpio 寫滿管線（64 KiB）後就永遠卡住，輔助程式要到
/// `TimeoutStartSec`（2.5 小時）才被 systemd 結束（tests/systemd 實測）。因此先釋放 cpio 的 `Command`、
/// cpio 失敗時結束 rpm2cpio，再等待它
fn rpm_extract(rpm2cpio: &Path, package: &Path, cpio: &Path, dir: &Path) -> Output {
    use std::process::{Command, Stdio};
    let spawned = Command::new(rpm2cpio)
        .arg(package)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return Output {
                status: None,
                stdout: String::new(),
                stderr: format!("{}: {e}", rpm2cpio.display()),
            };
        }
    };
    let Some(pipe) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Output::default();
    };
    let extracted = {
        let mut c = Command::new(cpio);
        c.args(["-idm", "--quiet", "--no-absolute-filenames"])
            .current_dir(dir)
            .stdin(pipe);
        c.output()
        // c（與管線的讀取端）在這裡釋放
    };
    let cpio_ok = matches!(&extracted, Ok(o) if o.status.success());
    if !cpio_ok {
        let _ = child.kill();
    }
    let rpm2cpio_err = child
        .stderr
        .take()
        .map(|mut e| {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut e, &mut s);
            s
        })
        .unwrap_or_default();
    let rpm2cpio_status = child.wait().ok().and_then(|s| s.code());
    match extracted {
        Ok(o) if o.status.success() && rpm2cpio_status != Some(0) => Output {
            status: rpm2cpio_status.or(Some(1)),
            stdout: String::new(),
            stderr: format!("rpm2cpio: {}", rpm2cpio_err.trim()),
        },
        Ok(o) => Output {
            status: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Output {
            status: None,
            stdout: String::new(),
            stderr: format!("{}: {e}", cpio.display()),
        },
    }
}

/// `systemctl show --property=…` 的輸出
pub fn parse_show(text: &str) -> ServiceState {
    let mut s = ServiceState::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "ActiveState" => s.active = v.trim().to_string(),
            "NRestarts" => s.n_restarts = v.trim().parse().unwrap_or(0),
            "MainPID" => s.main_pid = v.trim().parse().unwrap_or(0),
            "ExecMainStatus" => s.exit_status = v.trim().parse().ok(),
            _ => {}
        }
    }
    s
}

#[cfg(test)]
mod tests;
