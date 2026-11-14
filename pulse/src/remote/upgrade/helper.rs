//! 特權輔助程式 `pulse upgrade apply --request <檔案>`（`crates/kairos-proto/REMOTE.md` 第 4.4.1～4.5、17.9 節）。
//!
//! 由 root 的 `pulse-upgrade.service`（`pulse-upgrade.path` 在 `request.json` 出現時啟動）以**目前已安裝的**
//! 執行檔執行，不連網路。步驟（交易紀錄的 `step`，[`txn::Step`]）：
//!
//! 1. 檢查路徑（安裝目錄、root 的工作目錄與上層目錄的擁有者、權限、符號連結），讀進行中交易的指標：有
//!    尚未結束的交易就只接續那一筆，不看 `request.json` 的內容。
//! 2. `preparing`：記下替換前的狀態（自己的版本與 commit、偵測到的安裝方式、執行檔的雜湊、套件資料庫），
//!    把 `request.json` 指到的檔案**複製**到 root 專用的 `/var/lib/pulse-upgrade/<upgrade_id>/`（0700；只接受
//!    一般檔案、`O_NOFOLLOW`、`st_nlink == 1`、大小上限），重新驗證（信任錨與 `[upgrade]` 只從 root 擁有的
//!    `pulse.toml` 讀）：簽章、`SHA256SUMS`、檔名與平台、版本政策；預檢：把新的執行檔解到
//!    `/opt/pulse/.upgrade-<id>/`，以 `pulse` 帳號執行 `pulse --version`（必須是目標版本）與
//!    `pulse check-config --json --with-remote`（結束代碼 0／8）。不通過：沒有變更。
//! 3. `prepared`：驗證過的複本（新的執行檔、新 unit、回滾套件）與它們的雜湊都已記錄 → 寫入指標 →
//!    `replacing`。**紀錄先於指標，指標先於任何變更。**
//! 4. `replacing`：保留 `pulse.prev`（記錄）→ tar 備份 unit（記錄）→ 替換（tar：改名取代；deb：
//!    `dpkg -i --force-confold`；rpm：`rpm -U`；套件資料庫被鎖住時每 30 秒重試、最多 10 分鐘）→ tar 換上
//!    新 unit → `replaced`。`systemctl restart pulse.service`。
//! 5. `replaced`：健康確認（`health_timeout_secs`）：服務 `active`、`NRestarts` 沒有增加、狀態檔由新的行程
//!    寫入（`pid`、版本相符）、已註冊且有 report 被確認（`last_ack_at` 晚於重新啟動），再穩定 60 秒。結束
//!    代碼 2／3／4 或崩潰重啟立即判定失敗。
//! 6. 失敗則回滾：先記下意圖（`rollback_intent`）再還原——tar 換回 `pulse.prev`（與 unit）；deb／rpm 有驗證
//!    過的回滾套件時以套件管理員裝回，否則換回 `pulse.prev`（套件資料庫仍為新版，`detail` 註明）——
//!    `rolling_back`：以 `max(期限, 300 秒)` 確認舊版恢復：成功 `rolled_back`（`PLS-W8407`），失敗
//!    `rollback_failed`（`PLS-E8408`，保留所有紀錄，不再動作）。
//! 7. `done`：結果寫 `<狀態目錄>/upgrade/result.json`，刪除 `request.json` 與暫存（`rollback_failed` 時保留
//!    暫存），清理工作目錄，刪除指標。
//!
//! 每一步都先寫進 `state.json`（耐久）才做下一步；重新開機後 `pulse-upgrade.path` 再次觸發時，依紀錄的
//! 步驟與**磁碟上的實況**（執行檔、unit、`pulse.prev` 的雜湊，套件資料庫）冪等地接續，只使用 root 工作目錄
//! 裡驗證過的複本，不重讀服務帳號可寫的暫存區。每個當機點的結果見 REMOTE.md 第 17.9.4 節。外部程式只有
//! 固定路徑的 `systemctl`、`dpkg`、`dpkg-deb`、`dpkg-query`、`rpm`、`rpm2cpio`、`cpio`、`tar`，參數由程式
//! 組成（[`System`]）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kairos_proto::mgmt::{
    UPGRADE_STATE_FAILED, UPGRADE_STATE_ROLLBACK_FAILED, UPGRADE_STATE_ROLLED_BACK,
    UPGRADE_STATE_SUCCEEDED,
};
use kairos_proto::release::{
    self, Arch, ArtifactExpectation, Os, PolicyDecline, Product, ReleaseBundle, TrustPolicy,
    Version,
};

use super::files::{self, Request, UpgradeResult};
use crate::codes;
use crate::config_manager::AppConfig;

mod reqdir;
mod txn;

use reqdir::ReqDir;
pub use txn::ACTIVE_FILE;
use txn::{Cause, Loaded, Record, RollbackIntent, Step, UnitPlan, Verified, Via};

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

/// 開啟 feature `fault-injection` 的執行檔才有的標記字串（REMOTE.md 第 17.9.9 節）：打包腳本以它確認
/// 正式發佈的執行檔沒有故障注入的程式
#[cfg(feature = "fault-injection")]
pub const FAULT_INJECTION_MARKER: &str = "pulse-fault-injection-build";

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
    /// 套件資料庫中 `pulse` 的狀態（交易紀錄的 `pkg_db_before`／`pkg_db_after`）：deb 為
    /// `dpkg-query -W -f='${Version} ${db:Status-Abbrev}' pulse`，rpm 為
    /// `rpm -q --qf '%{VERSION}-%{RELEASE}' pulse`
    PackageVersion { method: String },
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
    /// 故障注入點（REMOTE.md 第 17.9.4、17.9.9 節）：交易的每一步之間呼叫。正式環境不做任何事；測試用的
    /// 實作在指定的點中止這次執行
    fn checkpoint(&self, _name: &str) {}
}

/// 路徑（正式環境為固定位置；測試時以暫存目錄模擬）
#[derive(Debug, Clone)]
pub struct Paths {
    pub request: PathBuf,
    pub install_dir: PathBuf,
    pub work_root: PathBuf,
    pub unit_path: PathBuf,
    /// 信任錨、安裝目錄與工作目錄必須由 root 擁有（測試時 `false`：執行測試的帳號也可以）
    pub require_root_owner: bool,
    /// 這個輔助程式的版本（＝目前安裝的版本）
    pub current_version: String,
    /// 這個輔助程式的 commit（`PULSE_BUILD_COMMIT`）
    pub current_commit: String,
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
            current_commit: env!("PULSE_BUILD_COMMIT").to_string(),
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

    /// 目前的執行檔
    fn binary(&self) -> PathBuf {
        self.install_dir.join("pulse")
    }

    /// 替換前保留的執行檔
    fn prev(&self) -> PathBuf {
        self.install_dir.join("pulse.prev")
    }
}

/// `request.json` 的 `from_version` 與輔助程式自己認定的不符時，結果 `detail` 的開頭
pub const DETAIL_FROM_VERSION_MISMATCH: &str = "from_version_mismatch";
/// 接續時 `request.json` 的 `from_version` 與紀錄的不同：沿用紀錄的版本，結果 `detail` 加註
pub const NOTE_FROM_VERSION_IGNORED: &str = "request_from_version_ignored";
/// `request.json` 的 `method` 與輔助程式自己偵測到的安裝方式不符時，結果 `detail` 的開頭
pub const DETAIL_METHOD_MISMATCH: &str = "method_mismatch";
/// 交易紀錄損毀、遺失或與系統的實況矛盾時，結果 `detail` 的開頭
pub const DETAIL_STATE_UNUSABLE: &str = "state_unusable";
/// 替換前無法寫入交易紀錄或指標時，結果 `detail` 的開頭
pub const DETAIL_STATE_NOT_SAVED: &str = "state_not_saved";
/// 進行中交易的指標讀不到或內容不合法時，結果 `detail` 的開頭
pub const DETAIL_POINTER_UNUSABLE: &str = "transaction_pointer_unusable";
/// 安裝目錄或 root 的工作目錄沒有通過路徑檢查時，結果 `detail` 的開頭
pub const DETAIL_UNSAFE_PATH: &str = "unsafe_path";
/// 工作目錄裡驗證過的複本與紀錄的雜湊不符時，結果 `detail` 的開頭
pub const DETAIL_COPY_CHANGED: &str = "verified_copy_changed";
/// 替換無法完成、已還原並確認舊版恢復時，結果 `detail` 的結尾
pub const DETAIL_RESTORED: &str = "restored";
/// `request.json` 的 `upgrade_id` 不是進行中的交易：接續進行中的交易，結果 `detail` 加註
pub const NOTE_UPGRADE_ID_IGNORED: &str = "request_upgrade_id_ignored";
/// 接續時 `request.json` 的 `method`、`target` 或 `rollback_artifact` 與紀錄的不同：沿用紀錄的內容
pub const NOTE_REQUEST_FIELDS_IGNORED: &str = "request_fields_ignored";
/// 接續時 `request.json` 讀不到或不合法：只依紀錄接續
pub const NOTE_REQUEST_UNREADABLE: &str = "request_unreadable";
/// tar：unit 被修改過（或這次沒有換上新的 unit）
pub const NOTE_UNIT_NOT_UPDATED: &str = "unit_not_updated";
/// tar：回滾時無法還原 unit（備份與紀錄的雜湊不符）
pub const NOTE_UNIT_NOT_RESTORED: &str = "unit_not_restored";

/// 日誌：stderr（journal）與工作目錄的 `upgrade.log`
struct Log {
    file: Option<std::fs::File>,
}

impl Log {
    fn new(path: Option<&Path>) -> Self {
        let file = path.and_then(|p| {
            let mut o = std::fs::OpenOptions::new();
            o.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.custom_flags(libc::O_NOFOLLOW);
            }
            o.open(p).ok()
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
        // `from_version`／`to_version` 來自服務帳號寫的 request.json（`validate` 已限制成合法版本），
        // 寫進 result.json 前再套一次 sanitize 與長度上限作為縱深防禦（安全稽核 2026-11-14）
        from_version: super::truncate_detail(&req.from_version),
        to_version: super::truncate_detail(&req.target.version),
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

/// Pulse 在 hello 之前的路徑預檢（`install.upgrade = "unsafe_path"`；REMOTE.md 第 2.1、17.9.7 節）：與
/// [`apply`] 第一步相同的檢查（安裝目錄、執行檔、工作目錄的根目錄與它們的上層目錄），但不建立、不修改
/// 任何東西，服務帳號因權限不足而看不到的部分略過。`Err` 的內容是結果 `detail` 中 `unsafe_path: ` 之後的
/// 文字（`<路徑>: <原因>`）。細節見 [`txn::precheck`]
pub fn precheck_paths(
    install_dir: &Path,
    work_root: &Path,
    require_root_owner: bool,
) -> Result<(), String> {
    txn::precheck(install_dir, work_root, txn::Owner::new(require_root_owner))
}

/// 不做任何變更就拒絕這次請求（路徑不安全、指標或紀錄不可用）：寫出結果、刪除 `request.json`（避免
/// `pulse-upgrade.path` 反覆觸發）。`id`：結果要回報的 `upgrade_id`（進行中的交易；`None` 時為請求的）。
/// 請求本身讀不到時無法產生結果，回傳 `Err`
fn refuse(
    paths: &Paths,
    reqdir: &ReqDir,
    request_name: &str,
    req: Result<Request, String>,
    id: Option<&str>,
    code: &str,
    detail: String,
) -> Result<UpgradeResult, String> {
    let mut req = match req {
        Ok(r) => r,
        Err(e) => {
            let _ = reqdir.remove_file(request_name);
            return Err(format!("{e}（另外：{detail}）"));
        }
    };
    let mut log = Log::new(None);
    if let Some(id) = id {
        if req.upgrade_id != id {
            req.upgrade_id = id.to_string();
        }
        let work = paths.work_root.join(id);
        if std::fs::symlink_metadata(&work).is_ok_and(|m| m.file_type().is_dir()) {
            log = Log::new(Some(&work.join(txn::LOG_FILE)));
        }
    }
    let r = result(&req, UPGRADE_STATE_FAILED, Some(code), detail);
    log.line("ERROR", &format!("升級失敗（{code}）：{}", r.detail));
    if let Ok(bytes) = serde_json::to_vec_pretty(&r) {
        let _ = reqdir.write_atomic(files::RESULT_FILE, &bytes, 0o644);
    }
    let _ = reqdir.remove_file(request_name);
    Ok(r)
}

/// 執行一次請求（或接續上次中斷的交易）。無法讀取請求、又沒有可接續的交易時回傳 `Err`（刪除
/// `request.json`，避免 `pulse-upgrade.path` 反覆觸發）
pub fn apply(paths: &Paths, sys: &dyn System) -> Result<UpgradeResult, String> {
    // 0. 請求目錄（服務帳號可寫）：一開始就以 O_DIRECTORY|O_NOFOLLOW 開啟取得 fd，之後對它的所有操作
    //    （讀 request.json、寫 result.json、刪 request.json 與暫存子目錄）都相對這個 fd（安全稽核
    //    2026-11-14 A1；REMOTE.md 第 17.9.7 節）。開不起來（已被換成符號連結、不是目錄等）就什麼都不碰、
    //    不寫結果——這種請求目錄沒有合法的升級，交由人檢查
    let upgrade_dir = paths.upgrade_dir();
    let request_name = paths
        .request
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(files::REQUEST_FILE);
    let reqdir = match ReqDir::open(&upgrade_dir) {
        Ok(d) => d,
        Err(why) => {
            eprintln!("錯誤：無法安全地開啟升級請求目錄：{why}");
            return Err(format!("{DETAIL_UNSAFE_PATH}: {why}"));
        }
    };
    let req = reqdir
        .read_file(request_name, files::MAX_REQUEST_BYTES)
        .and_then(|bytes| Request::from_untrusted_bytes(&bytes));
    let owner = txn::Owner::new(paths.require_root_owner);
    // 1. 路徑（REMOTE.md 第 17.9.7 節）：既存的目錄同樣檢查，不只依賴 systemd 第一次建立
    let mut warnings = Vec::new();
    match txn::check_install(&paths.install_dir, owner)
        .and_then(|()| txn::ensure_private_dir(&paths.work_root, owner))
    {
        Ok(w) => warnings.extend(w),
        Err(why) => {
            return refuse(
                paths,
                &reqdir,
                request_name,
                req,
                None,
                codes::E_UPGRADE_PRECHECK,
                format!("{DETAIL_UNSAFE_PATH}: {why}"),
            );
        }
    }
    // 2. 進行中的交易（第 17.9.5 節）：工作目錄不由 request.json 的 upgrade_id 決定。有尚未結束的交易時
    //    只接續那一筆——服務帳號在交易進行中改寫或換掉 request.json，都不會讓輔助程式開一筆新的交易
    let active = match txn::read_active(&paths.work_root) {
        Ok(a) => a,
        Err(why) => {
            // 指標不可用：不知道是否有進行中的交易，不開新的交易也不猜（保留指標，由人檢查）
            return refuse(
                paths,
                &reqdir,
                request_name,
                req,
                None,
                codes::E_UPGRADE_INTEGRITY,
                format!("{DETAIL_POINTER_UNUSABLE}: {why}; manual check required"),
            );
        }
    };
    if let Some(a) = active {
        let work = paths.work_root.join(&a.upgrade_id);
        let unusable =
            |why: String| format!("{DETAIL_STATE_UNUSABLE}: {why}; manual check required");
        let why = match txn::load_record(&work) {
            Loaded::V2(rec) => {
                if rec.upgrade_id != a.upgrade_id || rec.binding_sha256 != a.binding_sha256 {
                    Some(format!(
                        "{} does not belong to the transaction named by {ACTIVE_FILE}",
                        txn::STATE_FILE
                    ))
                } else if rec.step == Step::Preparing {
                    Some(format!(
                        "{} is at step preparing but {ACTIVE_FILE} exists",
                        txn::STATE_FILE
                    ))
                } else if rec.step == Step::Done && rec.result.is_none() {
                    Some(format!("{} is done without a result", txn::STATE_FILE))
                } else if rec.step == Step::Done
                    && !req.as_ref().is_ok_and(|r| r.upgrade_id == rec.upgrade_id)
                {
                    // 上一筆交易已結束（結果已寫出、request.json 已刪除），只差清除指標；現在的請求是
                    // 另一筆：清除指標後照常處理（不覆寫 result.json）
                    if let Err(e) = txn::clear_active(&paths.work_root) {
                        return refuse(
                            paths,
                            &reqdir,
                            request_name,
                            req,
                            None,
                            codes::E_UPGRADE_INTEGRITY,
                            format!(
                                "{DETAIL_POINTER_UNUSABLE}: {ACTIVE_FILE} cannot be removed: {e}; manual check required"
                            ),
                        );
                    }
                    warnings.push(format!(
                        "上一筆交易 {} 已結束，清除留下的 {ACTIVE_FILE}",
                        a.upgrade_id
                    ));
                    None
                } else {
                    return Ok(resume(
                        paths,
                        &reqdir,
                        request_name,
                        sys,
                        *rec,
                        req.ok(),
                        work,
                        warnings,
                    ));
                }
            }
            Loaded::Missing => Some(format!("{} is missing", txn::STATE_FILE)),
            Loaded::Legacy(_) => Some(format!("{} has the legacy schema", txn::STATE_FILE)),
            Loaded::Unusable(why) => Some(why),
        };
        if let Some(why) = why {
            // 指標保留：之後的請求同樣被擋下，檢查後刪除 active.json 與那個工作目錄
            return refuse(
                paths,
                &reqdir,
                request_name,
                req,
                Some(&a.upgrade_id),
                codes::E_UPGRADE_INTEGRITY,
                unusable(why),
            );
        }
    }
    // 3. 沒有指標：任何「已動手而未結束」或無法判斷的紀錄都不得被當成不存在
    let mut legacy_open = Vec::new();
    for e in txn::scan(&paths.work_root) {
        let why = match &e.kind {
            txn::Kind::V2 { step, .. } if step.unfinished() => Some(format!(
                "transaction {} is at step {} but {ACTIVE_FILE} is missing",
                e.id,
                step.name()
            )),
            txn::Kind::Unusable(why) => Some(format!("{}: {why}", e.id)),
            txn::Kind::Legacy { done: false, .. } => {
                legacy_open.push(e);
                None
            }
            _ => None,
        };
        if let Some(why) = why {
            return refuse(
                paths,
                &reqdir,
                request_name,
                req,
                None,
                codes::E_UPGRADE_INTEGRITY,
                format!("{DETAIL_STATE_UNUSABLE}: {why}; manual check required"),
            );
        }
    }
    if legacy_open.len() > 1 {
        let ids: Vec<&str> = legacy_open.iter().map(|e| e.id.as_str()).collect();
        return refuse(
            paths,
            &reqdir,
            request_name,
            req,
            None,
            codes::E_UPGRADE_INTEGRITY,
            format!(
                "{DETAIL_STATE_UNUSABLE}: more than one unfinished legacy transaction ({}); manual check required",
                ids.join(", ")
            ),
        );
    }
    let req = match req {
        Ok(r) => r,
        Err(e) => {
            let _ = reqdir.remove_file(request_name);
            return Err(e);
        }
    };
    if let Some(e) = legacy_open.pop() {
        // 由 0.9.0 的輔助程式開始的交易（v1，沒有指標）：第 17.9.6 節
        let Loaded::Legacy(state) = txn::load_record(&e.path) else {
            return refuse(
                paths,
                &reqdir,
                request_name,
                Ok(req),
                None,
                codes::E_UPGRADE_INTEGRITY,
                format!(
                    "{DETAIL_STATE_UNUSABLE}: {}: {} changed while it was being read; manual check required",
                    e.id,
                    txn::STATE_FILE
                ),
            );
        };
        return resume_legacy(
            paths,
            &reqdir,
            request_name,
            sys,
            &e.id,
            *state,
            req,
            e.path,
            warnings,
        );
    }
    // 4. 全新的請求（或還沒動手就中斷的同一筆請求）
    let work = paths.work_root.join(&req.upgrade_id);
    let work_exists = std::fs::symlink_metadata(&work).is_ok();
    let stale = |why: String| {
        refuse(
            paths,
            &reqdir,
            request_name,
            Ok(req.clone()),
            None,
            codes::E_UPGRADE_INTEGRITY,
            format!("{DETAIL_STATE_UNUSABLE}: {why}; manual check required"),
        )
    };
    match txn::load_record(&work) {
        Loaded::Missing => {
            let left = if work_exists {
                txn::traces(&work)
            } else {
                Vec::new()
            };
            if !left.is_empty() {
                return stale(format!(
                    "{} is missing but {} exists",
                    txn::STATE_FILE,
                    left.join(", ")
                ));
            }
        }
        Loaded::V2(rec) => match rec.step {
            Step::Done => {
                if let Some(r) = rec.result {
                    // 同一個 upgrade_id 的請求再次出現：回報記下的結果
                    let mut h =
                        Helper::new(paths, &reqdir, request_name, sys, req, work, Vec::new());
                    h.finish(&r);
                    return Ok(r);
                }
                return stale(format!("{} is done without a result", txn::STATE_FILE));
            }
            Step::Preparing | Step::Prepared => {
                // 沒有指標：還沒動手。確認系統仍是紀錄的替換前狀態才重來（輔助程式已是另一個版本時，
                // 那筆紀錄屬於更早、已無關的嘗試）
                if rec.from_version == paths.current_version && !rec.old_binary_sha256.is_empty() {
                    let now = txn::sha256_regular(&paths.binary()).unwrap_or_default();
                    if now != rec.old_binary_sha256 {
                        return stale(format!(
                            "{} is at step {} but the installed binary has changed",
                            txn::STATE_FILE,
                            rec.step.name()
                        ));
                    }
                    let db = package_db(sys, &rec.method);
                    if db != rec.pkg_db_before {
                        return stale(format!(
                            "{} is at step {} but the package database has changed ({:?} → {:?})",
                            txn::STATE_FILE,
                            rec.step.name(),
                            rec.pkg_db_before,
                            db
                        ));
                    }
                }
                warnings.push(format!(
                    "上一次執行停在 {}（還沒有任何變更）：重新開始",
                    rec.step.name()
                ));
            }
            // 已在上面的掃描擋下；到這裡代表紀錄在兩次讀取之間變了
            _ => {
                return stale(format!(
                    "{} is at step {} but {ACTIVE_FILE} is missing",
                    txn::STATE_FILE,
                    rec.step.name()
                ));
            }
        },
        Loaded::Legacy(state) => match state.result {
            Some(r) if state.step == txn::LegacyStep::Done => {
                let mut h = Helper::new(paths, &reqdir, request_name, sys, req, work, Vec::new());
                h.finish(&r);
                return Ok(r);
            }
            _ => return stale(format!("{} has the legacy schema", txn::STATE_FILE)),
        },
        Loaded::Unusable(why) => return stale(why),
    }
    // 第 17.9.8 節：開始新的交易之前清理（這裡沒有進行中的交易）
    let removed = txn::prune(&paths.work_root, &[req.upgrade_id.as_str()], None);
    match txn::ensure_private_dir(&work, owner) {
        Ok(w) => warnings.extend(w),
        Err(why) => {
            return refuse(
                paths,
                &reqdir,
                request_name,
                Ok(req),
                None,
                codes::E_UPGRADE_PRECHECK,
                format!("{DETAIL_UNSAFE_PATH}: {why}"),
            );
        }
    }
    txn::reset(&work);
    let mut h = Helper::new(paths, &reqdir, request_name, sys, req, work, warnings);
    if !removed.is_empty() {
        h.log.line(
            "INFO",
            &format!("已清除較早的工作目錄：{}", removed.join("、")),
        );
    }
    let r = h.begin();
    h.finish(&r);
    Ok(r)
}

/// 套件資料庫中 `pulse` 的狀態（tar 與查不到時 `None`）
fn package_db(sys: &dyn System, method: &str) -> Option<String> {
    if !matches!(method, "deb" | "rpm") {
        return None;
    }
    let out = sys.run(&Cmd::PackageVersion {
        method: method.to_string(),
    });
    out.ok()
        .then(|| out.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 接續 v2 的交易：之後 `request.json` 的內容不再有任何作用（只比對並加註），一切以紀錄為準
#[allow(clippy::too_many_arguments)]
fn resume(
    paths: &Paths,
    reqdir: &ReqDir,
    request_name: &str,
    sys: &dyn System,
    mut rec: Record,
    req: Option<Request>,
    work: PathBuf,
    mut warnings: Vec<String>,
) -> UpgradeResult {
    let trusted = Request {
        schema: files::REQUEST_SCHEMA.to_string(),
        upgrade_id: rec.upgrade_id.clone(),
        target: rec.target.clone(),
        method: rec.method.clone(),
        artifact: rec
            .artifact
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_default(),
        manifest: rec.manifest.clone(),
        rollback_artifact: rec.rollback.as_ref().map(|r| r.name.clone()),
        health_timeout_secs: u32::try_from(rec.health_timeout_secs).unwrap_or(u32::MAX),
        allow_downgrade: false,
        from_version: rec.from_version.clone(),
        requested_at: req
            .as_ref()
            .map(|r| r.requested_at.clone())
            .unwrap_or_default(),
    };
    let mut notes = Vec::new();
    match &req {
        None => {
            warnings.push("request.json 讀不到或不合法：只依交易紀錄接續".to_string());
            notes.push(NOTE_REQUEST_UNREADABLE);
        }
        Some(r) => {
            if r.upgrade_id != trusted.upgrade_id {
                warnings.push(format!(
                    "request.json 的 upgrade_id（{}）不是進行中的交易（{}）：接續進行中的交易，不開新的交易",
                    r.upgrade_id, trusted.upgrade_id
                ));
                notes.push(NOTE_UPGRADE_ID_IGNORED);
            }
            if r.from_version != trusted.from_version {
                warnings.push(format!(
                    "request.json 的 from_version（{}）與替換前記下的版本（{}）不同，以後者為準",
                    r.from_version, trusted.from_version
                ));
                notes.push(NOTE_FROM_VERSION_IGNORED);
            }
            let mut changed = Vec::new();
            if r.method != trusted.method {
                changed.push(format!("method {} → {}", r.method, trusted.method));
            }
            if r.target != trusted.target {
                changed.push(format!(
                    "target {} → {}",
                    r.target.version, trusted.target.version
                ));
            }
            // 只比對通過驗證而被記下的回滾套件（請求附了回滾檔但沒有通過驗證時，紀錄中沒有它，不算被改過）
            if !rec.legacy
                && trusted.rollback_artifact.is_some()
                && r.rollback_artifact != trusted.rollback_artifact
            {
                changed.push("rollback_artifact".to_string());
            }
            if !changed.is_empty() {
                warnings.push(format!(
                    "request.json 的內容與交易紀錄不同（{}），以紀錄為準",
                    changed.join("、")
                ));
                notes.push(NOTE_REQUEST_FIELDS_IGNORED);
            }
        }
    }
    for n in notes {
        if !rec.notes.iter().any(|x| x == n) {
            rec.notes.push(n.to_string());
        }
    }
    // 進行中交易的工作目錄同樣要通過檢查
    let owner = txn::Owner::new(paths.require_root_owner);
    let mut h = match txn::ensure_private_dir(&work, owner) {
        Ok(w) => {
            warnings.extend(w);
            Helper::new(paths, reqdir, request_name, sys, trusted, work, warnings)
        }
        Err(why) => {
            let mut h = Helper::new_with(
                paths,
                reqdir,
                request_name,
                sys,
                trusted,
                work,
                warnings,
                false,
            );
            h.keep_active = true;
            let r = h.fail(
                codes::E_UPGRADE_PRECHECK,
                format!("{DETAIL_UNSAFE_PATH}: {why}; manual check required"),
            );
            h.finish(&r);
            return r;
        }
    };
    let r = match rec.step {
        Step::Prepared | Step::Replacing => {
            h.log.line(
                "INFO",
                &format!(
                    "接續上次中斷的替換（{}）：以工作目錄裡驗證過的複本與磁碟上的實況繼續",
                    rec.step.name()
                ),
            );
            rec.step = Step::Replacing;
            match h.try_save(&mut rec) {
                Ok(()) => h.replacing(rec),
                Err(e) => h.replace_failed(rec, format!("{DETAIL_STATE_NOT_SAVED}: {e}")),
            }
        }
        Step::Replaced => {
            h.log
                .line("INFO", "接續上次中斷的升級：已替換，重新做健康確認");
            if !rec.service_restarted {
                h.log
                    .line("INFO", "上次還沒有重新啟動 pulse.service：現在重新啟動");
                h.restart(&mut rec);
            }
            h.after_replace(rec)
        }
        Step::RollbackIntent => {
            h.log.line("INFO", "接續上次中斷的回滾：重新還原");
            h.restore_and_confirm(rec, true)
        }
        Step::RollingBack => {
            h.log.line("INFO", "接續上次中斷的回滾");
            if !rec.service_restarted {
                h.log
                    .line("INFO", "上次還沒有重新啟動 pulse.service：現在重新啟動");
                h.restart(&mut rec);
            }
            let reason = resumed_reason(&rec);
            h.confirm_rollback(rec, reason)
        }
        // `done` 而沒有結果、`preparing` 已在 apply 擋下
        Step::Done | Step::Preparing => match rec.result.clone() {
            Some(r) => r,
            None => {
                h.keep_active = true;
                h.fail(
                    codes::E_UPGRADE_INTEGRITY,
                    format!(
                        "{DETAIL_STATE_UNUSABLE}: unexpected step {}; manual check required",
                        rec.step.name()
                    ),
                )
            }
        },
    };
    h.finish(&r);
    r
}

/// 接續回滾時結果的原因：「<原本的原因>; resumed」
fn resumed_reason(rec: &Record) -> String {
    match rec.rollback_intent.as_ref().map(|i| i.reason.as_str()) {
        Some(r) if !r.is_empty() => format!("{r}; resumed"),
        _ => "resumed".to_string(),
    }
}

/// 接續由 0.9.0 的輔助程式開始的交易（v1 的狀態檔；REMOTE.md 第 17.9.6 節）：只接受已替換之後的步驟，
/// 轉成 v2 的紀錄並寫入指標，之後依 v2 的規則
#[allow(clippy::too_many_arguments)]
fn resume_legacy(
    paths: &Paths,
    reqdir: &ReqDir,
    request_name: &str,
    sys: &dyn System,
    id: &str,
    s: txn::LegacyState,
    req: Request,
    work: PathBuf,
    mut warnings: Vec<String>,
) -> Result<UpgradeResult, String> {
    let mut notes: Vec<String> = s.notes.clone();
    let mut note = |n: &str| {
        if !notes.iter().any(|x| x == n) {
            notes.push(n.to_string());
        }
    };
    if req.upgrade_id != id {
        warnings.push(format!(
            "request.json 的 upgrade_id（{}）不是進行中的交易（{id}）：接續進行中的交易，不開新的交易",
            req.upgrade_id
        ));
        note(NOTE_UPGRADE_ID_IGNORED);
    }
    // 0.9.0 的狀態檔沒有記版本、安裝方式與目標：只能沿用請求中的值（那一次升級由 0.9.0 的輔助程式開始，
    // 行為與 0.9.0 相同）；有記的以紀錄為準
    let from_version = if s.from_version.is_empty() {
        warnings.push(
            "狀態檔沒有記錄替換前的版本與安裝方式（由 0.9.0 的輔助程式寫入），沿用請求中的值"
                .to_string(),
        );
        req.from_version.clone()
    } else {
        if req.from_version != s.from_version {
            note(NOTE_FROM_VERSION_IGNORED);
        }
        s.from_version.clone()
    };
    let recorded = !s.method.is_empty();
    let method = if recorded {
        s.method.clone()
    } else {
        req.method.clone()
    };
    let target = s.target.clone().unwrap_or_else(|| req.target.clone());
    let rollback_name = if recorded {
        s.rollback_artifact.clone()
    } else {
        req.rollback_artifact.clone()
    };
    if recorded
        && (req.method != method || req.target != target || req.rollback_artifact != rollback_name)
    {
        note(NOTE_REQUEST_FIELDS_IGNORED);
    }
    let timeout = if s.health_timeout_secs > 0 {
        s.health_timeout_secs
    } else {
        u64::from(kairos_proto::mgmt::effective_health_timeout_secs(
            req.health_timeout_secs,
            0,
        ))
    };
    let mut rec = Record::new(id, &from_version, "", &method);
    rec.legacy = true;
    rec.target = target;
    rec.prev_saved = true;
    rec.health_timeout_secs = timeout;
    rec.deadline_unix = s.deadline_unix;
    rec.n_restarts_before = s.n_restarts_before;
    rec.restarted_unix = s.restarted_unix;
    if work.join(txn::ROLLBACK_PKG_FILE).is_file() {
        rec.rollback = Some(Verified {
            name: rollback_name.unwrap_or_else(|| txn::ROLLBACK_PKG_FILE.to_string()),
            sha256: String::new(),
            signer: String::new(),
        });
    }
    if s.unit_replaced {
        rec.unit = Some(UnitPlan {
            old_sha256: String::new(),
            new_sha256: String::new(),
        });
        rec.unit_backed_up = true;
        rec.unit_replaced = true;
    }
    drop(note);
    rec.notes = notes;
    let mut restart = false;
    rec.step = match s.step {
        txn::LegacyStep::Replaced => Step::Replaced,
        txn::LegacyStep::RollingBack => {
            rec.rollback_intent = Some(RollbackIntent {
                reason: s.rollback_reason.clone(),
                cause: Cause::Health,
                via: Via::Binary,
            });
            Step::RollingBack
        }
        txn::LegacyStep::Verified if paths.current_version == rec.target.version => {
            // v1 沒有「替換中」：輔助程式自己已是目標版本，代表執行檔已替換（舊版的輔助程式一定先保留
            // pulse.prev 才替換）、還沒記下 replaced。視為已替換、尚未重新啟動，重新起算健康確認的期限
            restart = true;
            Step::Replaced
        }
        txn::LegacyStep::Verified | txn::LegacyStep::Done => {
            return refuse(
                paths,
                reqdir,
                request_name,
                Ok(req),
                Some(id),
                codes::E_UPGRADE_INTEGRITY,
                format!(
                    "{DETAIL_STATE_UNUSABLE}: legacy {} is at step verified and this helper ({}) is not the target version ({}); manual check required",
                    txn::STATE_FILE,
                    paths.current_version,
                    rec.target.version
                ),
            );
        }
    };
    let trusted = Request {
        upgrade_id: id.to_string(),
        target: rec.target.clone(),
        method: rec.method.clone(),
        rollback_artifact: rec.rollback.as_ref().map(|r| r.name.clone()),
        from_version: rec.from_version.clone(),
        ..req
    };
    let owner = txn::Owner::new(paths.require_root_owner);
    match txn::ensure_private_dir(&work, owner) {
        Ok(w) => warnings.extend(w),
        Err(why) => {
            return refuse(
                paths,
                reqdir,
                request_name,
                Ok(trusted),
                Some(id),
                codes::E_UPGRADE_PRECHECK,
                format!("{DETAIL_UNSAFE_PATH}: {why}; manual check required"),
            );
        }
    }
    let mut h = Helper::new(paths, reqdir, request_name, sys, trusted, work, warnings);
    h.log.line(
        "INFO",
        &format!(
            "接續由舊版輔助程式開始的交易（狀態檔 v1，步驟 {:?}）：轉成 v2 的紀錄",
            s.step
        ),
    );
    if restart {
        rec.n_restarts_before = sys.service_state().map(|s| s.n_restarts).unwrap_or(0);
        rec.restarted_unix = sys.now_unix();
        rec.deadline_unix = rec.restarted_unix + i64::try_from(timeout).unwrap_or(i64::MAX);
    }
    // v1 沒有記下重新啟動是否已送出：replaced／rolling_back 照 v1 的行為（不再送）；只有「已替換、尚未
    // 記錄」需要現在重新啟動
    rec.service_restarted = !restart;
    rec.binding_sha256 = rec.compute_binding();
    // 轉換後的紀錄與指標寫不進去時仍繼續（系統已被替換，不能半途停下）
    h.save(&mut rec);
    if let Err(e) = txn::write_active(&paths.work_root, id, &rec.binding_sha256) {
        h.log.line("WARN", &e);
    }
    let r = match rec.step {
        Step::RollingBack => {
            h.log.line("INFO", "接續上次中斷的回滾");
            let reason = resumed_reason(&rec);
            h.confirm_rollback(rec, reason)
        }
        _ => {
            h.log
                .line("INFO", "接續上次中斷的升級：已替換，重新做健康確認");
            if !rec.service_restarted {
                h.restart(&mut rec);
            }
            h.after_replace(rec)
        }
    };
    h.finish(&r);
    Ok(r)
}

struct Helper<'a> {
    paths: &'a Paths,
    /// 請求目錄（服務帳號可寫）的 fd：寫 `result.json`、刪 `request.json` 與暫存子目錄、讀暫存的發佈檔
    /// 都相對它（安全稽核 2026-11-14 A1）
    reqdir: &'a ReqDir,
    /// 請求檔的檔名（`request.json`，相對 [`Helper::reqdir`]）
    request_name: &'a str,
    sys: &'a dyn System,
    /// 可信的請求內容：新交易在 [`Helper::begin`] 比對過 `from_version` 與 `method`；接續時由紀錄組成
    req: Request,
    work: PathBuf,
    log: Log,
    /// 還沒有紀錄可記時先放在這裡的說明（寫入紀錄的 `notes`，最後附在結果的 `detail`）
    pending_notes: Vec<String>,
    /// 結束時保留進行中交易的指標（紀錄不可用，需要人工檢查）
    keep_active: bool,
    /// 結果（`done`）沒有寫進紀錄：結束時刪除紀錄，避免它被當成未結束的交易
    done_unsaved: bool,
}

impl<'a> Helper<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        paths: &'a Paths,
        reqdir: &'a ReqDir,
        request_name: &'a str,
        sys: &'a dyn System,
        req: Request,
        work: PathBuf,
        warnings: Vec<String>,
    ) -> Self {
        Self::new_with(paths, reqdir, request_name, sys, req, work, warnings, true)
    }

    /// `log_file = false`：工作目錄沒有通過檢查，日誌只寫 stderr
    #[allow(clippy::too_many_arguments)]
    fn new_with(
        paths: &'a Paths,
        reqdir: &'a ReqDir,
        request_name: &'a str,
        sys: &'a dyn System,
        req: Request,
        work: PathBuf,
        warnings: Vec<String>,
        log_file: bool,
    ) -> Self {
        let log_path = work.join(txn::LOG_FILE);
        let mut log = Log::new(log_file.then_some(log_path.as_path()));
        log.line(
            "INFO",
            &format!(
                "pulse upgrade apply：升級 {}（{} → {}，{}）",
                req.upgrade_id, req.from_version, req.target.version, req.method
            ),
        );
        for w in warnings {
            log.line("WARN", &w);
        }
        Self {
            paths,
            reqdir,
            request_name,
            sys,
            req,
            work,
            log,
            pending_notes: Vec::new(),
            keep_active: false,
            done_unsaved: false,
        }
    }

    /// 寫入交易紀錄。替換之後的步驟寫不進去時只能記下來繼續（不能半途停下；這次執行仍會做完健康確認與
    /// 回滾，接續時依磁碟上的實況冪等重做）
    fn save(&mut self, rec: &mut Record) {
        if let Err(e) = self.try_save(rec) {
            self.log.line("WARN", &e);
        }
    }

    /// 寫入交易紀錄；失敗時回錯誤（替換前：不得繼續）
    fn try_save(&mut self, rec: &mut Record) -> Result<(), String> {
        rec.save(&self.work)
    }

    fn fail(&mut self, code: &str, detail: String) -> UpgradeResult {
        self.log
            .line("ERROR", &format!("升級失敗（{code}）：{detail}"));
        result(&self.req, UPGRADE_STATE_FAILED, Some(code), detail)
    }

    /// 把結果記進紀錄（`done`）
    fn done(&mut self, mut rec: Record, r: UpgradeResult) -> UpgradeResult {
        rec.step = Step::Done;
        rec.result = Some(r.clone());
        if let Err(e) = self.try_save(&mut rec) {
            self.log.line("WARN", &e);
            self.done_unsaved = true;
        }
        self.sys.checkpoint("record_done");
        r
    }

    fn sha(&self, path: &Path) -> String {
        txn::sha256_regular(path).unwrap_or_default()
    }

    fn is_package(&self) -> bool {
        matches!(self.req.method.as_str(), "deb" | "rpm")
    }

    /// 新的交易（REMOTE.md 第 17.9.3 節）：`preparing` → 複製、驗證、預檢 → `prepared` → 指標 →
    /// `replacing` → 替換。
    ///
    /// `request.json` 由服務帳號寫入、不可信（第 5.1、17.8 節）。決定「怎麼替換、回滾到哪一版」的欄位以
    /// 輔助程式自己可信的來源為準，請求中的值只用來比對：`from_version` 必須等於輔助程式自己的版本
    /// （＝目前安裝的版本），`method` 必須等於輔助程式自己偵測到的安裝方式；不符就拒絕（什麼都不改）。
    /// 否則取得服務帳號的人可以謊報較舊的 `from_version`、附上該舊版有官方簽章的套件當回滾檔，再讓新版
    /// 的健康確認失敗，使 root 以 `dpkg -i`／`rpm -U --oldpackage` 裝回任一個舊版，繞過本機的
    /// `allow_downgrade = false`
    fn begin(&mut self) -> UpgradeResult {
        let claimed = self.req.from_version.clone();
        let trusted = self.paths.current_version.clone();
        if claimed != trusted {
            self.req.from_version = trusted.clone();
            return self.fail(
                codes::E_UPGRADE_INTEGRITY,
                format!(
                    "{DETAIL_FROM_VERSION_MISMATCH}: request says {claimed}, installed version is {trusted}"
                ),
            );
        }
        if self.req.method != self.paths.method {
            let detail = format!(
                "{DETAIL_METHOD_MISMATCH}: request says {}, detected install method is {}",
                self.req.method, self.paths.method
            );
            return self.fail(codes::E_UPGRADE_INTEGRITY, detail);
        }
        self.sys.checkpoint("before_record");
        // preparing：替換前的狀態
        let mut rec = Record::new(
            &self.req.upgrade_id,
            &self.paths.current_version,
            &self.paths.current_commit,
            &self.req.method,
        );
        rec.target = self.req.target.clone();
        rec.old_binary_sha256 = self.sha(&self.paths.binary());
        rec.pkg_db_before = package_db(self.sys, &self.req.method);
        rec.notes = std::mem::take(&mut self.pending_notes);
        if rec.old_binary_sha256.is_empty() {
            return self.fail(
                codes::E_UPGRADE_REPLACE,
                format!("無法讀取 {}", self.paths.binary().display()),
            );
        }
        if let Err(e) = self.try_save(&mut rec) {
            return self.fail(
                codes::E_UPGRADE_REPLACE,
                format!("{DETAIL_STATE_NOT_SAVED}: {e}"),
            );
        }
        self.sys.checkpoint("record_preparing");
        if let Err(r) = self.prepare(&mut rec) {
            return self.done(rec, *r);
        }
        // prepared：之後綁定的欄位不再改變
        rec.step = Step::Prepared;
        rec.binding_sha256 = rec.compute_binding();
        if let Err(e) = self.try_save(&mut rec) {
            let r = self.fail(
                codes::E_UPGRADE_REPLACE,
                format!("{DETAIL_STATE_NOT_SAVED}: {e}"),
            );
            return self.done(rec, r);
        }
        self.sys.checkpoint("record_prepared");
        // 紀錄先於指標，指標先於任何變更：之後到交易結束為止，輔助程式只接續這一筆
        if let Err(e) =
            txn::write_active(&self.paths.work_root, &rec.upgrade_id, &rec.binding_sha256)
        {
            let r = self.fail(
                codes::E_UPGRADE_REPLACE,
                format!("{DETAIL_STATE_NOT_SAVED}: {e}"),
            );
            return self.done(rec, r);
        }
        self.sys.checkpoint("pointer_written");
        rec.step = Step::Replacing;
        if let Err(e) = self.try_save(&mut rec) {
            return self.replace_failed(rec, format!("{DETAIL_STATE_NOT_SAVED}: {e}"));
        }
        self.sys.checkpoint("record_replacing");
        self.replacing(rec)
    }

    /// 複製、重新驗證、預檢；把驗證過的複本與它們的雜湊記進 `rec`（還不寫入：由呼叫端寫成 `prepared`）
    fn prepare(&mut self, rec: &mut Record) -> Result<(), Box<UpgradeResult>> {
        let req = self.req.clone();
        // 1. 複製。暫存的發佈檔在服務帳號可寫的 `<請求目錄>/<upgrade_id>/`：相對請求目錄的 fd 開啟那個
        //    子目錄（不跟隨符號連結），再相對它讀每個檔案，之後重新驗證（安全稽核 2026-11-14 A1）
        let staged = match self.reqdir.subdir(&req.upgrade_id) {
            Ok(d) => d,
            Err(e) => return Err(Box::new(self.fail(codes::E_UPGRADE_INTEGRITY, e))),
        };
        let copy = |this: &mut Self, name: &str, max: u64| -> Result<PathBuf, Box<UpgradeResult>> {
            let dst = this.work.join(name);
            match staged.copy_file(name, &dst, max) {
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
        rec.artifact = Some(Verified {
            name: req.artifact.clone(),
            sha256: actual,
            signer: verified.signer.fingerprint.clone(),
        });
        rec.manifest = req.manifest.clone();
        // 回滾檔：以它所屬發佈的 SHA256SUMS（本機驗證過的紀錄）重新驗證
        rec.rollback = None;
        let _ = std::fs::remove_file(self.work.join(txn::ROLLBACK_PKG_FILE));
        if let (Some(r), Some((s, g))) = (&rollback, &rollback_sums) {
            let rs = read(s);
            let rg = read(g);
            let name = req.rollback_artifact.clone().unwrap_or_default();
            let sha = files::sha256_file(r).unwrap_or_default();
            let signer = release::verify_bundle(
                &ReleaseBundle {
                    sha256sums: &rs,
                    signature: &rg,
                    manifest: None,
                },
                &trust,
            )
            .ok()
            .and_then(|v| {
                // 版本以輔助程式自己的版本（替換前的版本）為準，不是請求中的值
                let e = ArtifactExpectation {
                    product: Some(Product::Pulse),
                    version: Some(&req.from_version),
                    commit: None,
                    os: Some(Os::Linux),
                    arch,
                    format,
                };
                v.check_artifact(&name, &sha, &e)
                    .ok()
                    .map(|_| v.signer.fingerprint.clone())
            });
            // 驗證過的回滾套件留在工作目錄（接續時只用這一份）。複製而不是改名：重新安裝同一版時回滾檔
            // 與發佈檔是同一個檔名
            let dst = self.work.join(txn::ROLLBACK_PKG_FILE);
            let kept = signer.and_then(|signer| {
                txn::copy_durable(r, &dst, 0o600)
                    .ok()
                    .filter(|()| self.sha(&dst) == sha)
                    .map(|()| Verified {
                        name: name.clone(),
                        sha256: sha.clone(),
                        signer,
                    })
            });
            if kept.is_none() {
                self.log
                    .line("WARN", "回滾檔沒有通過驗證，回滾時改以 pulse.prev 回復");
                let _ = std::fs::remove_file(&dst);
            }
            if name != req.artifact {
                let _ = std::fs::remove_file(r);
            }
            rec.rollback = kept;
        }
        self.log.line(
            "INFO",
            &format!(
                "已重新驗證 {}（簽署者 {}）",
                req.artifact, verified.signer.fingerprint
            ),
        );
        // 3. 預檢
        self.remove_precheck_dirs();
        let xdir = self
            .paths
            .install_dir
            .join(format!(".upgrade-{}", req.upgrade_id));
        if let Err(e) = crate::remote::create_dir_all_mode(&xdir, 0o755) {
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_PRECHECK,
                format!("無法建立 {}：{e}", xdir.display()),
            )));
        }
        let prechecked = self.precheck(rec, &artifact, &xdir, verified.manifest.as_ref());
        self.remove_precheck_dirs();
        prechecked?;
        rec.health_timeout_secs = u64::from(kairos_proto::mgmt::effective_health_timeout_secs(
            req.health_timeout_secs,
            config.upgrade.health_timeout_secs,
        ));
        // 複本都寫入磁碟後才會記成 prepared
        if let Err(e) = txn::fsync_dir(&self.work) {
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_REPLACE,
                format!("{DETAIL_STATE_NOT_SAVED}: {e}"),
            )));
        }
        Ok(())
    }

    /// 預檢（解開、`--version`、`check-config`），並把新的執行檔（tar）與要換上的 unit 保存到工作目錄
    fn precheck(
        &mut self,
        rec: &mut Record,
        artifact: &Path,
        xdir: &Path,
        manifest: Option<&release::ReleaseManifest>,
    ) -> Result<(), Box<UpgradeResult>> {
        let req = self.req.clone();
        let cmd = match req.method.as_str() {
            "tar" => Cmd::TarExtract {
                archive: artifact.to_path_buf(),
                dir: xdir.to_path_buf(),
            },
            "deb" => Cmd::DebExtract {
                package: artifact.to_path_buf(),
                dir: xdir.to_path_buf(),
            },
            _ => Cmd::RpmExtract {
                package: artifact.to_path_buf(),
                dir: xdir.to_path_buf(),
            },
        };
        let out = self.sys.run(&cmd);
        if !out.ok() {
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_PRECHECK,
                format!("無法解開發佈檔：{}", out.last_line()),
            )));
        }
        let (new_binary, new_unit) = match locate_binary(&req.method, xdir) {
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
        // 新的執行檔：雜湊是之後判斷「是否已替換」的依據；tar 另外保存一份（接續時不再解開發佈檔）
        rec.new_binary_sha256 = self.sha(&new_binary);
        let saved = if req.method == "tar" {
            let dst = self.work.join(txn::NEW_BINARY_FILE);
            txn::copy_durable(&new_binary, &dst, 0o755)
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    if self.sha(&dst) == rec.new_binary_sha256 {
                        Ok(())
                    } else {
                        Err("複本與來源不符".to_string())
                    }
                })
        } else {
            Ok(())
        };
        if rec.new_binary_sha256.is_empty() || saved.is_err() {
            return Err(Box::new(self.fail(
                codes::E_UPGRADE_PRECHECK,
                format!(
                    "無法保存新的執行檔：{}",
                    saved.err().unwrap_or_else(|| "讀取失敗".to_string())
                ),
            )));
        }
        // tar：發佈清單的 unit 雜湊與目前的 unit 不同、而目前的 unit 是歷來官方版本之一（沒被改過）時換新
        rec.unit = None;
        if req.method == "tar"
            && let (Some(u), Some(new_unit)) = (manifest.and_then(|m| m.unit.as_ref()), &new_unit)
        {
            let current = self.sha(&self.paths.unit_path);
            if current == u.sha256 {
                // 已是新版的 unit
            } else if !current.is_empty()
                && u.previous_sha256.contains(&current)
                && self.sha(new_unit) == u.sha256
                && txn::copy_durable(new_unit, &self.work.join(txn::NEW_UNIT_FILE), 0o644).is_ok()
            {
                rec.unit = Some(UnitPlan {
                    old_sha256: current,
                    new_sha256: u.sha256.clone(),
                });
            } else {
                if !rec.notes.iter().any(|n| n == NOTE_UNIT_NOT_UPDATED) {
                    rec.notes.push(NOTE_UNIT_NOT_UPDATED.to_string());
                }
                self.log.line(
                    "WARN",
                    &format!(
                        "{} 被修改過或不存在，不換上新版的 unit（{}）",
                        self.paths.unit_path.display(),
                        codes::W_UPGRADE_UNSUPPORTED
                    ),
                );
            }
        }
        Ok(())
    }

    /// 刪除預檢用的 `/opt/pulse/.upgrade-*`（包含先前中斷留下的）
    fn remove_precheck_dirs(&mut self) {
        for e in std::fs::read_dir(&self.paths.install_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            if e.file_name().to_string_lossy().starts_with(".upgrade-")
                && std::fs::symlink_metadata(e.path()).is_ok_and(|m| m.file_type().is_dir())
            {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    /// `replacing`：替換，成功後重新啟動並做健康確認；無法完成時依系統是否已被改過決定失敗或還原
    fn replacing(&mut self, mut rec: Record) -> UpgradeResult {
        match self.replace(&mut rec) {
            Ok(()) => {
                self.log.line(
                    "INFO",
                    &format!(
                        "已替換為 {}，重新啟動 pulse.service",
                        self.req.target.version
                    ),
                );
                self.restart(&mut rec);
                self.after_replace(rec)
            }
            Err(e) => self.replace_failed(rec, e),
        }
    }

    /// `systemctl restart pulse.service`，並記下已經送出（接續時不必再送）。在記下之前中斷的話接續時會再
    /// 送一次：多重新啟動一次沒有副作用（`NRestarts` 只計自動重啟）
    fn restart(&mut self, rec: &mut Record) {
        if let Err(e) = self.sys.restart_service() {
            self.log
                .line("WARN", &format!("systemctl restart 失敗：{e}"));
        }
        rec.service_restarted = true;
        self.save(rec);
    }

    /// 步驟 4：保留舊版、替換（REMOTE.md 第 17.9.3 節的 ①～⑤）。第一次執行與接續走同一段：每個動作都先看
    /// 磁碟上的實況（執行檔、`pulse.prev`、unit 的雜湊）再決定要不要做，所以在任何一點中斷後重做都不會
    /// 把舊版的備份蓋掉，也不會重複套用。只使用工作目錄裡驗證過的複本
    fn replace(&mut self, rec: &mut Record) -> Result<(), String> {
        let current = self.paths.binary();
        let prev = self.paths.prev();
        let old = rec.old_binary_sha256.clone();
        let new = rec.new_binary_sha256.clone();
        let not_saved = |e: String| format!("{DETAIL_STATE_NOT_SAVED}: {e}");
        // 驗證過的複本沒有變
        let source = if self.is_package() {
            let name = rec.artifact.as_ref().map(|a| a.name.clone());
            let path = self.work.join(name.unwrap_or_default());
            let want = rec.artifact.as_ref().map(|a| a.sha256.clone());
            if want.as_deref() != Some(self.sha(&path).as_str()) {
                return Err(format!(
                    "{DETAIL_COPY_CHANGED}: {} does not match the recorded SHA-256",
                    path.display()
                ));
            }
            path
        } else {
            let path = self.work.join(txn::NEW_BINARY_FILE);
            if new.is_empty() || self.sha(&path) != new {
                return Err(format!(
                    "{DETAIL_COPY_CHANGED}: {} does not match the recorded SHA-256",
                    path.display()
                ));
            }
            path
        };
        // ① pulse.prev：只在目前的執行檔還是舊版時保留；已保留（雜湊相符）就不再覆寫
        let cur = self.sha(&current);
        let prev_is_old = self.sha(&prev) == old;
        if rec.prev_saved && prev_is_old {
            if cur != old {
                self.log
                    .line("INFO", "pulse.prev 已是替換前的執行檔，不再覆寫");
            }
        } else if cur == old {
            copy_executable(&current, &prev).map_err(|e| format!("無法保留目前的執行檔：{e}"))?;
            if self.sha(&prev) != old {
                return Err("保留的 pulse.prev 與替換前的執行檔不符".to_string());
            }
            self.sys.checkpoint("prev_copied");
            rec.prev_saved = true;
            self.try_save(rec).map_err(not_saved)?;
        } else if prev_is_old {
            rec.prev_saved = true;
            self.try_save(rec).map_err(not_saved)?;
        } else {
            return Err(
                "目前的執行檔已不是替換前的版本，pulse.prev 也不是：沒有可用的備份".to_string(),
            );
        }
        self.sys.checkpoint("prev_recorded");
        // ② tar：unit 先備份並記錄
        let mut replace_unit = false;
        if let Some(u) = rec.unit.clone() {
            let backup = self.work.join(txn::UNIT_BACKUP_FILE);
            let backup_ok = self.sha(&backup) == u.old_sha256;
            if rec.unit_backed_up && backup_ok {
                replace_unit = true;
            } else if self.sha(&self.paths.unit_path) == u.old_sha256 {
                txn::copy_durable(&self.paths.unit_path, &backup, 0o600)
                    .map_err(|e| format!("無法備份 {}：{e}", self.paths.unit_path.display()))?;
                if self.sha(&backup) != u.old_sha256 {
                    return Err("unit 的備份與來源不符".to_string());
                }
                self.sys.checkpoint("unit_backed_up");
                rec.unit_backed_up = true;
                self.try_save(rec).map_err(not_saved)?;
                replace_unit = true;
            } else if backup_ok {
                rec.unit_backed_up = true;
                self.try_save(rec).map_err(not_saved)?;
                replace_unit = true;
            } else {
                // unit 在預檢之後被改過、也沒有備份：不動它
                if !rec.notes.iter().any(|n| n == NOTE_UNIT_NOT_UPDATED) {
                    rec.notes.push(NOTE_UNIT_NOT_UPDATED.to_string());
                }
                self.log.line(
                    "WARN",
                    &format!(
                        "{} 在預檢之後被改過，不換上新版的 unit",
                        self.paths.unit_path.display()
                    ),
                );
            }
            self.sys.checkpoint("unit_backup_recorded");
        }
        // ③ 執行檔
        let before = self.sys.service_state().map(|s| s.n_restarts).unwrap_or(0);
        match self.req.method.as_str() {
            "tar" => {
                if self.sha(&current) != new {
                    // 寫到同目錄的暫存檔、fsync 後改名取代（原子操作），再 fsync 目錄
                    copy_executable(&source, &current)
                        .map_err(|e| format!("無法替換執行檔：{e}"))?;
                }
            }
            // 套件管理員可以重複執行：接續時（可能中斷在安裝到一半）再執行一次
            "deb" => self.package(&Cmd::DpkgInstall { package: source })?,
            _ => self.package(&Cmd::RpmInstall { package: source })?,
        }
        if self.sha(&current) != new {
            return Err("替換後的執行檔與驗證過的發佈檔不符".to_string());
        }
        if self.is_package() {
            rec.pkg_db_after = package_db(self.sys, &self.req.method);
        }
        self.sys.checkpoint("binary_replaced");
        // ④ tar：換上新 unit（依雜湊判斷是否已換過）
        if replace_unit && let Some(u) = rec.unit.clone() {
            let ok = self.sha(&self.paths.unit_path) == u.new_sha256
                || (self.sha(&self.work.join(txn::NEW_UNIT_FILE)) == u.new_sha256
                    && txn::copy_durable(
                        &self.work.join(txn::NEW_UNIT_FILE),
                        &self.paths.unit_path,
                        0o644,
                    )
                    .is_ok());
            if ok {
                if let Err(e) = self.sys.daemon_reload() {
                    self.log.line("WARN", &format!("daemon-reload 失敗：{e}"));
                }
                self.sys.checkpoint("unit_replaced");
                rec.unit_replaced = true;
                self.log
                    .line("INFO", "已換上新版的 pulse.service（沒有被修改過）");
            } else {
                if !rec.notes.iter().any(|n| n == NOTE_UNIT_NOT_UPDATED) {
                    rec.notes.push(NOTE_UNIT_NOT_UPDATED.to_string());
                }
                self.log.line("WARN", "無法換上新版的 pulse.service");
            }
        }
        // ⑤ replaced
        rec.step = Step::Replaced;
        rec.service_restarted = false;
        rec.n_restarts_before = before;
        rec.restarted_unix = self.sys.now_unix();
        rec.deadline_unix =
            rec.restarted_unix + i64::try_from(rec.health_timeout_secs).unwrap_or(i64::MAX);
        self.save(rec);
        self.sys.checkpoint("record_replaced");
        Ok(())
    }

    /// 系統是否已被這筆交易改過（執行檔、unit、套件資料庫與紀錄的替換前狀態不同）
    fn touched(&mut self, rec: &Record) -> bool {
        if self.sha(&self.paths.binary()) != rec.old_binary_sha256 {
            return true;
        }
        if let Some(u) = &rec.unit
            && self.sha(&self.paths.unit_path) != u.old_sha256
        {
            return true;
        }
        self.is_package() && package_db(self.sys, &self.req.method) != rec.pkg_db_before
    }

    /// 替換無法完成（REMOTE.md 第 17.9.3 節）：系統沒被改過 → `failed`；已被改過 → 還原（換回
    /// `pulse.prev`、tar 另還原 unit）並確認舊版恢復，結果仍是 `failed`（`restored`）或 `rollback_failed`
    fn replace_failed(&mut self, rec: Record, e: String) -> UpgradeResult {
        if !self.touched(&rec) {
            let r = self.fail(codes::E_UPGRADE_REPLACE, e);
            return self.done(rec, r);
        }
        self.log.line(
            "WARN",
            &format!(
                "替換無法完成（{e}），系統已被改過：還原到 {}",
                self.req.from_version
            ),
        );
        self.rollback(rec, format!("replace_failed: {e}"), Cause::ReplaceFailed)
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

    /// 步驟 5：健康確認；失敗則回滾
    fn after_replace(&mut self, rec: Record) -> UpgradeResult {
        if !rec.legacy && self.sha(&self.paths.binary()) != rec.new_binary_sha256 {
            self.log.line(
                "WARN",
                "目前的執行檔與這筆交易換上的不同（替換後被改過）：健康確認仍以目標版本判斷",
            );
        }
        let now = self.sys.now_unix();
        let remaining = Duration::from_secs(u64::try_from(rec.deadline_unix - now).unwrap_or(0));
        let target = self.req.target.version.clone();
        let check = if remaining.is_zero() {
            Err("health_timeout".to_string())
        } else {
            self.health(&target, &rec, remaining)
        };
        match check {
            Ok(()) => {
                let detail = rec.notes.join(",");
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
                self.done(rec, r)
            }
            Err(reason) => {
                self.log.line(
                    "WARN",
                    &format!("健康確認失敗（{reason}），回滾到 {}", self.req.from_version),
                );
                self.rollback(rec, reason, Cause::Health)
            }
        }
    }

    /// 步驟 6：回滾。**先記下意圖（原因與方式）再還原**：還原途中中斷時，接續依紀錄的方式冪等重做
    fn rollback(&mut self, mut rec: Record, reason: String, cause: Cause) -> UpgradeResult {
        // 替換無法完成時一律換回 pulse.prev（與先前「替換失敗時換回執行檔」相同，不使用回滾套件）
        let via = if cause == Cause::Health
            && self.is_package()
            && rec.rollback.is_some()
            && self.work.join(txn::ROLLBACK_PKG_FILE).is_file()
        {
            Via::Package
        } else {
            Via::Binary
        };
        if via == Via::Binary && self.is_package() && cause == Cause::Health {
            let note = format!(
                "binary_restored_package_db_shows_{}",
                self.req.target.version
            );
            if !rec.notes.contains(&note) {
                rec.notes.push(note);
            }
        }
        rec.step = Step::RollbackIntent;
        rec.rollback_intent = Some(RollbackIntent { reason, cause, via });
        self.save(&mut rec);
        self.sys.checkpoint("rollback_intent_recorded");
        self.restore_and_confirm(rec, false)
    }

    /// `rollback_intent`：還原 → `rolling_back` → 重新啟動 → 確認舊版恢復
    fn restore_and_confirm(&mut self, mut rec: Record, resumed: bool) -> UpgradeResult {
        let intent = rec.rollback_intent.clone().unwrap_or(RollbackIntent {
            reason: String::new(),
            cause: Cause::Health,
            via: Via::Binary,
        });
        let reason = if resumed {
            resumed_reason(&rec)
        } else {
            intent.reason.clone()
        };
        if let Err(e) = self.restore(&mut rec, intent.via) {
            return self.rollback_failed(rec, format!("{reason}; rollback: {e}"));
        }
        self.sys.checkpoint("rollback_restored");
        rec.step = Step::RollingBack;
        rec.service_restarted = false;
        rec.n_restarts_before = self.sys.service_state().map(|s| s.n_restarts).unwrap_or(0);
        rec.restarted_unix = self.sys.now_unix();
        let timeout = Duration::from_secs(rec.health_timeout_secs).max(MIN_ROLLBACK_TIMEOUT);
        rec.deadline_unix =
            rec.restarted_unix + i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX);
        self.save(&mut rec);
        self.sys.checkpoint("record_rolling_back");
        self.restart(&mut rec);
        self.confirm_rollback(rec, reason)
    }

    /// 還原（可重複執行）：`package` 以驗證過的 `rollback.pkg` 經套件管理員裝回；`binary` 換回
    /// `pulse.prev`（必須等於紀錄的替換前執行檔），tar 另還原 unit
    fn restore(&mut self, rec: &mut Record, via: Via) -> Result<(), String> {
        let current = self.paths.binary();
        let prev = self.paths.prev();
        match via {
            Via::Package => {
                let pkg = self.work.join(txn::ROLLBACK_PKG_FILE);
                let want = rec
                    .rollback
                    .as_ref()
                    .map(|r| r.sha256.clone())
                    .unwrap_or_default();
                // 由 v1 轉換的紀錄沒有雜湊：照 v1 的行為使用
                if !want.is_empty() && self.sha(&pkg) != want {
                    return Err(format!(
                        "{DETAIL_COPY_CHANGED}: {} does not match the recorded SHA-256",
                        pkg.display()
                    ));
                }
                self.log.line(
                    "INFO",
                    &format!(
                        "以回滾檔 {} 經套件管理員裝回 {}（{}）",
                        self.req
                            .rollback_artifact
                            .as_deref()
                            .unwrap_or(txn::ROLLBACK_PKG_FILE),
                        self.req.from_version,
                        if self.req.method == "deb" {
                            "dpkg -i"
                        } else {
                            "rpm -U --oldpackage --replacepkgs"
                        }
                    ),
                );
                let cmd = if self.req.method == "deb" {
                    Cmd::DpkgInstall { package: pkg }
                } else {
                    Cmd::RpmInstall { package: pkg }
                };
                self.package(&cmd)
            }
            Via::Binary => {
                self.log.line(
                    "INFO",
                    &format!(
                        "以替換前保留的 {} 換回 {}{}",
                        prev.display(),
                        self.req.from_version,
                        if self.is_package() {
                            "（不經套件管理員，套件資料庫仍顯示新版）"
                        } else {
                            ""
                        }
                    ),
                );
                let old = rec.old_binary_sha256.clone();
                let prev_sha = self.sha(&prev);
                if prev_sha.is_empty() {
                    return Err(format!("無法換回 pulse.prev：{} 不存在", prev.display()));
                }
                if !old.is_empty() && prev_sha != old {
                    return Err(
                        "無法換回 pulse.prev：它與這筆交易記下的替換前執行檔不符".to_string()
                    );
                }
                if self.sha(&current) != prev_sha {
                    copy_executable(&prev, &current)
                        .map_err(|e| format!("無法換回 pulse.prev：{e}"))?;
                }
                self.sys.checkpoint("rollback_restoring");
                if let Some(u) = rec.unit.clone()
                    && rec.unit_backed_up
                {
                    let backup = self.work.join(txn::UNIT_BACKUP_FILE);
                    let backup_sha = self.sha(&backup);
                    // 依雜湊判斷（不依賴 unit_replaced：換上新 unit 之後、記錄之前可能中斷）
                    let already = if u.old_sha256.is_empty() {
                        false
                    } else {
                        self.sha(&self.paths.unit_path) == u.old_sha256
                    };
                    if already {
                        // unit 沒被換過，或已還原
                    } else if !backup_sha.is_empty()
                        && (u.old_sha256.is_empty() || backup_sha == u.old_sha256)
                        && txn::copy_durable(&backup, &self.paths.unit_path, 0o644).is_ok()
                    {
                        let _ = self.sys.daemon_reload();
                    } else {
                        if !rec.notes.iter().any(|n| n == NOTE_UNIT_NOT_RESTORED) {
                            rec.notes.push(NOTE_UNIT_NOT_RESTORED.to_string());
                        }
                        self.log.line("WARN", "無法還原 pulse.service");
                    }
                }
                Ok(())
            }
        }
    }

    fn confirm_rollback(&mut self, rec: Record, reason: String) -> UpgradeResult {
        let remaining = Duration::from_secs(
            u64::try_from(rec.deadline_unix - self.sys.now_unix()).unwrap_or(0),
        );
        let from = self.req.from_version.clone();
        let cause = rec
            .rollback_intent
            .as_ref()
            .map_or(Cause::Health, |i| i.cause);
        match self.health(&from, &rec, remaining) {
            Ok(()) => {
                let mut detail = reason;
                if !rec.notes.is_empty() {
                    detail = format!("{detail}; {}", rec.notes.join(","));
                }
                let r = match cause {
                    Cause::Health => {
                        self.log
                            .line("WARN", &format!("已回滾到 {from}（{detail}）"));
                        result(
                            &self.req,
                            UPGRADE_STATE_ROLLED_BACK,
                            Some(codes::W_UPGRADE_ROLLED_BACK),
                            detail,
                        )
                    }
                    Cause::ReplaceFailed => {
                        if self.is_package() {
                            let db = package_db(self.sys, &self.req.method);
                            if db != rec.pkg_db_before {
                                detail = format!(
                                    "{detail}; package_db_shows_{}",
                                    db.as_deref().unwrap_or("unknown")
                                );
                            }
                        }
                        detail = format!("{detail}; {DETAIL_RESTORED}");
                        self.fail(codes::E_UPGRADE_REPLACE, detail)
                    }
                };
                self.done(rec, r)
            }
            Err(e) => self.rollback_failed(rec, format!("{reason}; after rollback: {e}")),
        }
    }

    fn rollback_failed(&mut self, rec: Record, detail: String) -> UpgradeResult {
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
        self.done(rec, r)
    }

    /// 健康確認（REMOTE.md 第 4.5.1 節）
    fn health(&mut self, version: &str, rec: &Record, timeout: Duration) -> Result<(), String> {
        let start = self.sys.mono();
        let status_path = self.status_path();
        let mut stable_since: Option<Duration> = None;
        let mut last = "not_started".to_string();
        loop {
            let elapsed = self.sys.mono().saturating_sub(start);
            match self.sys.service_state() {
                Ok(s) => {
                    if s.n_restarts > rec.n_restarts_before {
                        return Err(format!(
                            "crash_restart (NRestarts {} → {})",
                            rec.n_restarts_before, s.n_restarts
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
                                    .is_some_and(|t| t.timestamp() >= rec.restarted_unix);
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

    /// 步驟 7：寫結果、刪除請求與暫存、清理工作目錄、清除指標（每一步都可以重做）
    fn finish(&mut self, r: &UpgradeResult) {
        let bytes = serde_json::to_vec_pretty(r).unwrap_or_default();
        // 相對請求目錄的 fd 原子寫入 result.json（安全稽核 2026-11-14 A1）：服務帳號事後把請求目錄
        // 改名、換成符號連結，都改變不了 root 寫到哪裡
        if let Err(e) = self.reqdir.write_atomic(files::RESULT_FILE, &bytes, 0o644) {
            self.log
                .line("ERROR", &format!("無法寫入 {}：{e}", files::RESULT_FILE));
        }
        self.sys.checkpoint("result_written");
        if let Err(e) = self.reqdir.remove_file(self.request_name) {
            self.log.line("WARN", &format!("無法刪除請求檔：{e}"));
        }
        self.sys.checkpoint("request_removed");
        if !self.keep_active {
            self.remove_precheck_dirs();
            if r.state != UPGRADE_STATE_ROLLBACK_FAILED {
                // 相對 fd 遞迴刪除暫存的 `<upgrade_id>/` 子目錄（全程不跟隨符號連結）
                if let Err(e) = self.reqdir.remove_dir_all(&self.req.upgrade_id) {
                    self.log.line("WARN", &format!("無法刪除暫存目錄：{e}"));
                }
                // 已結束的交易只留紀錄與日誌
                txn::strip(&self.work);
            }
            if self.done_unsaved {
                // 結果寫不進紀錄：刪除紀錄（刪除不需要空間），否則它會被當成未結束的交易。連刪除都失敗
                // 時（檔案系統唯讀或損壞）指標照常清除：留下的未結束紀錄讓之後的請求以 state_unusable
                // 擋下，而不是在期限早已過去之後「接續」並回滾一個其實已完成的升級
                match std::fs::remove_file(self.work.join(txn::STATE_FILE)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => self.log.line(
                        "ERROR",
                        &format!(
                            "無法寫入也無法刪除 {}：{e}；需要人工檢查",
                            self.work.join(txn::STATE_FILE).display()
                        ),
                    ),
                }
            }
        }
        if !self.keep_active {
            // 第 17.9.8 節：只保留最近的紀錄（這一筆不動）
            let removed = txn::prune(
                &self.paths.work_root,
                &[self.req.upgrade_id.as_str()],
                Some(r.state.as_str()),
            );
            if !removed.is_empty() {
                self.log.line(
                    "INFO",
                    &format!("已清除較早的工作目錄：{}", removed.join("、")),
                );
            }
            // 交易結束：清除進行中交易的指標
            if let Err(e) = txn::clear_active(&self.paths.work_root) {
                self.log.line(
                    "WARN",
                    &format!(
                        "無法刪除 {}：{e}",
                        self.paths.work_root.join(ACTIVE_FILE).display()
                    ),
                );
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

/// 複製執行檔（0755）：寫到同目錄新建的暫存檔（不跟隨符號連結）→ fsync → 改名取代 → fsync 目錄。
/// 保留 `pulse.prev`、換上新的執行檔與換回 `pulse.prev` 都用它（來源不動）
fn copy_executable(src: &Path, dst: &Path) -> std::io::Result<()> {
    txn::copy_durable(src, dst, 0o755)
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

    /// 正式發佈的執行檔不做任何事；只有開啟 feature `fault-injection` 的測試用建置會在指定的點停住
    /// （REMOTE.md 第 17.9.9 節）
    fn checkpoint(&self, name: &str) {
        #[cfg(all(unix, feature = "fault-injection"))]
        fault_injection_hold(name);
        #[cfg(not(all(unix, feature = "fault-injection")))]
        let _ = name;
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
            Cmd::PackageVersion { method } => {
                let mut c = if method == "deb" {
                    let mut c =
                        Command::new(first_existing(&["/usr/bin/dpkg-query", "/bin/dpkg-query"]));
                    c.args(["-W", "-f=${Version} ${db:Status-Abbrev}", "pulse"]);
                    c
                } else {
                    let mut c = Command::new(first_existing(&["/usr/bin/rpm", "/bin/rpm"]));
                    c.args(["-q", "--qf", "%{VERSION}-%{RELEASE}", "pulse"]);
                    c
                };
                c.env("LC_ALL", "C");
                run_command(c)
            }
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

/// 故障注入（只在 feature `fault-injection`；`tests/systemd/` 的實機當機點）：`/var/lib/pulse-upgrade/
/// fault-inject`（只有 root 可寫）的內容是這個 checkpoint 的名稱時，把它改名為 `fault-inject.hit` 後停住，
/// 等測試腳本終止整台主機（`docker kill`）。改名讓它只觸發一次：重新開機後的接續不會再停
#[cfg(all(unix, feature = "fault-injection"))]
fn fault_injection_hold(name: &str) {
    let root = Path::new(WORK_ROOT);
    let hook = root.join("fault-inject");
    let Ok(text) = std::fs::read_to_string(&hook) else {
        return;
    };
    if text.trim() != name {
        return;
    }
    if std::fs::rename(&hook, root.join("fault-inject.hit")).is_err() {
        return;
    }
    let _ = txn::fsync_dir(root);
    eprintln!("{FAULT_INJECTION_MARKER}: holding at checkpoint {name}");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
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
mod fault_tests;
#[cfg(test)]
mod tests;
