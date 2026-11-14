//! 特權輔助程式的測試（REMOTE.md 第 12.1 節「輔助程式」、第 17.8、17.9 節）：以暫存目錄模擬 `/opt/pulse`、
//! `/var/lib/pulse`、`/var/lib/pulse-upgrade`，以假的 [`System`] 模擬 systemd、套件管理員與「替換後的
//! Pulse」（寫狀態檔、崩潰、連不上），時鐘也是假的（健康確認的 600 秒不必真的等）。
//!
//! 這個檔案另外提供 `fault_tests.rs`（當機點的故障注入矩陣）共用的假系統：[`Fake`] 可以在指定的
//! checkpoint 中止這次執行（[`apply_until`]），並在每個 checkpoint 檢查寫入順序的不變條件（紀錄先於
//! 指標、指標先於任何變更；`pulse.prev` 只會是替換前的執行檔）。

#![cfg(unix)]

use super::*;
use crate::remote::upgrade::testsign::{self, TestKey};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;

pub(super) const ID: &str = "9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70";
pub(super) const OTHER_ID: &str = "11111111-2222-4333-8444-555555555555";
pub(super) const OLD_UNIT: &str = "[Unit]\nDescription=Pulse 0.9.0\n";
pub(super) const NEW_UNIT: &str = "[Unit]\nDescription=Pulse 0.9.1\n";

pub(super) fn sha(b: &[u8]) -> String {
    files::hex(ring::digest::digest(&ring::digest::SHA256, b).as_ref())
}

pub(super) fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// 以 root 執行測試時（容器中）目錄權限擋不住寫入：依賴「寫不進去」的測試略過
pub(super) fn running_as_root() -> bool {
    // SAFETY: geteuid 沒有前置條件，也不會失敗
    unsafe { libc::geteuid() == 0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Behavior {
    Healthy,
    Crash,
    NoConnect,
    Exit(i32),
}

#[derive(Debug, Default, Clone)]
struct Svc {
    active: String,
    n_restarts: u64,
    pid: u32,
    exit: Option<i32>,
    restarted_at: Duration,
    /// 服務行程啟動時的執行檔版本（替換執行檔不會改變已在執行的行程）
    running: String,
}

/// [`Fake`] 在指定的 checkpoint 中止這次執行時的 panic 內容（模擬行程被終止：之後的程式都不會執行）
pub(super) struct Crashed(pub(super) String);

/// 交易開始前的系統狀態（寫入順序的不變條件以它判斷「系統是否已變更」）
#[derive(Debug, Clone)]
struct Baseline {
    binary: Option<Vec<u8>>,
    prev: Option<Vec<u8>>,
    unit: Option<Vec<u8>>,
    pkg_db: Option<String>,
}

/// checkpoint 的名稱與到達它時要呼叫的函式
pub(super) type CheckpointHook = (String, Box<dyn Fn()>);

/// 假的作業系統：執行檔的內容是 `VERSION=<版本>`，狀態檔由「服務行程的版本」依它的行為寫出
pub(super) struct Fake {
    install: PathBuf,
    status: PathBuf,
    work_root: PathBuf,
    unit: PathBuf,
    mono: Cell<Duration>,
    t0: i64,
    behavior: RefCell<HashMap<String, Behavior>>,
    svc: RefCell<Svc>,
    calls: RefCell<Vec<String>>,
    pub(super) dpkg_locks: Cell<u32>,
    /// 套件管理員一律失敗（不是被鎖住），不改變任何東西
    pub(super) pkg_fails: Cell<bool>,
    pub(super) check_config: Cell<i32>,
    pub(super) version_override: RefCell<Option<String>>,
    /// 預檢的最後一步（check-config）執行時呼叫：模擬「預檢之後、替換之前」發生的事
    pub(super) after_precheck: RefCell<Option<Box<dyn Fn()>>>,
    /// 重新啟動服務時呼叫（替換之後、健康確認之前）
    pub(super) on_restart: RefCell<Option<Box<dyn Fn()>>>,
    /// 到達指定的 checkpoint 時呼叫（不中止）
    pub(super) at_checkpoint: RefCell<Option<CheckpointHook>>,
    /// 套件資料庫中 pulse 的狀態（`<版本> ii`；安裝到一半為 `<版本> iF`）
    pub(super) pkg_db: RefCell<Option<String>>,
    crash_at: RefCell<Option<String>>,
    seen: RefCell<Vec<String>>,
    violations: RefCell<Vec<String>>,
    baseline: Baseline,
    /// 第幾次開機（[`Fake::reboot`] 加一）：`boot_id` 由它產生
    boot: u32,
    /// `boot_id` 讀不到（或模擬 0.10.0 的輔助程式：紀錄沒有 `boot_id`）
    pub(super) no_boot_id: Cell<bool>,
}

pub(super) fn binary_version(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .trim_start_matches("VERSION=")
        .to_string()
}

impl Fake {
    pub(super) fn new(root: &Path) -> Self {
        let install = root.join("opt/pulse");
        let unit = root.join("etc/systemd/system/pulse.service");
        let installed = binary_version(&install.join("pulse"));
        let pkg_db = Some(format!("{installed} ii"));
        let baseline = Baseline {
            binary: std::fs::read(install.join("pulse")).ok(),
            prev: std::fs::read(install.join("pulse.prev")).ok(),
            unit: std::fs::read(&unit).ok(),
            pkg_db: pkg_db.clone(),
        };
        Self {
            install,
            status: root.join("var/lib/pulse/status.json"),
            work_root: root.join("var/lib/pulse-upgrade"),
            unit,
            mono: Cell::new(Duration::ZERO),
            t0: chrono::Utc::now().timestamp(),
            behavior: RefCell::new(HashMap::new()),
            svc: RefCell::new(Svc {
                active: "active".into(),
                pid: 100,
                running: installed,
                ..Default::default()
            }),
            calls: RefCell::new(Vec::new()),
            dpkg_locks: Cell::new(0),
            pkg_fails: Cell::new(false),
            check_config: Cell::new(0),
            version_override: RefCell::new(None),
            after_precheck: RefCell::new(None),
            on_restart: RefCell::new(None),
            at_checkpoint: RefCell::new(None),
            pkg_db: RefCell::new(pkg_db),
            crash_at: RefCell::new(None),
            seen: RefCell::new(Vec::new()),
            violations: RefCell::new(Vec::new()),
            baseline,
            boot: 1,
            no_boot_id: Cell::new(false),
        }
    }

    /// 直接設定 `NRestarts`（模擬這次開機中服務先前自動重啟過的次數，或 root 執行 `reset-failed`）
    pub(super) fn set_n_restarts(&self, n: u64) {
        self.svc.borrow_mut().n_restarts = n;
    }

    fn successor(&self, secs: u64, rebooted: bool) -> Fake {
        let mut svc = self.svc.borrow().clone();
        svc.restarted_at = Duration::ZERO;
        if rebooted {
            // 開機時 systemd 以目前安裝的執行檔啟動服務；NRestarts 從 0 起算
            svc = Svc {
                active: "active".into(),
                n_restarts: 0,
                pid: svc.pid + 100,
                exit: None,
                restarted_at: Duration::ZERO,
                running: binary_version(&self.install.join("pulse")),
            };
        }
        Fake {
            install: self.install.clone(),
            status: self.status.clone(),
            work_root: self.work_root.clone(),
            unit: self.unit.clone(),
            mono: Cell::new(Duration::ZERO),
            t0: self.now_unix() + i64::try_from(secs).unwrap(),
            behavior: RefCell::new(self.behavior.borrow().clone()),
            svc: RefCell::new(svc),
            calls: RefCell::new(Vec::new()),
            dpkg_locks: Cell::new(0),
            pkg_fails: Cell::new(false),
            check_config: Cell::new(0),
            version_override: RefCell::new(None),
            after_precheck: RefCell::new(None),
            on_restart: RefCell::new(None),
            at_checkpoint: RefCell::new(None),
            pkg_db: RefCell::new(self.pkg_db.borrow().clone()),
            crash_at: RefCell::new(None),
            seen: RefCell::new(Vec::new()),
            violations: RefCell::new(Vec::new()),
            baseline: self.baseline.clone(),
            boot: self.boot + u32::from(rebooted),
            no_boot_id: Cell::new(self.no_boot_id.get()),
        }
    }

    /// 這次執行被中止後主機重新開機（30 秒後）：同一筆交易，另一個行程
    pub(super) fn reboot(&self) -> Fake {
        self.successor(30, true)
    }

    /// 重新開機，`secs` 秒後才回來（期限可能已過）
    pub(super) fn reboot_after(&self, secs: u64) -> Fake {
        self.successor(secs, true)
    }

    /// 只有輔助程式被終止（OOM、人工 `kill` 等），主機沒有重新開機、5 秒後被重新觸發：服務仍是原本的行程
    pub(super) fn helper_killed(&self) -> Fake {
        self.successor(5, false)
    }

    /// 只有輔助程式被終止、主機沒有重新開機，`secs` 秒後才再被觸發（期限可能已過）：服務仍是原本的行程
    pub(super) fn helper_killed_after(&self, secs: u64) -> Fake {
        self.successor(secs, false)
    }

    pub(super) fn set(&self, version: &str, b: Behavior) {
        self.behavior.borrow_mut().insert(version.into(), b);
    }

    pub(super) fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    /// 這次執行經過的 checkpoint
    pub(super) fn seen(&self) -> Vec<String> {
        self.seen.borrow().clone()
    }

    /// 每個 checkpoint 都符合寫入順序的不變條件
    pub(super) fn assert_invariants(&self) {
        let v = self.violations.borrow();
        assert!(v.is_empty(), "寫入順序的不變條件被破壞：{v:#?}");
    }

    fn restarts(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|c| *c == "restart")
            .count()
    }

    fn maybe_crash(&self, name: &str) {
        let hit = self.crash_at.borrow().as_deref() == Some(name);
        if hit {
            *self.crash_at.borrow_mut() = None;
            // resume_unwind 不經過 panic hook：測試輸出不會有多餘的 panic 訊息
            std::panic::resume_unwind(Box::new(Crashed(name.to_string())));
        }
    }

    /// 寫入順序的不變條件（REMOTE.md 第 17.9.3 節）：系統有任何變更時，指標與 `replacing` 之後的紀錄必定
    /// 已經寫入；指標存在時必定有一份屬於它的可用紀錄；`pulse.prev` 只會是交易前的內容或替換前的執行檔
    fn observe(&self, at: &str) {
        let read = |p: &Path| std::fs::read(p).ok();
        let bin = read(&self.install.join("pulse"));
        let prev = read(&self.install.join("pulse.prev"));
        let unit = read(&self.unit);
        let mut bad = Vec::new();
        if prev != self.baseline.prev && prev != self.baseline.binary {
            bad.push("pulse.prev 不是替換前的執行檔".to_string());
        }
        let changed = bin != self.baseline.binary
            || prev != self.baseline.prev
            || unit != self.baseline.unit
            || *self.pkg_db.borrow() != self.baseline.pkg_db;
        match txn::read_active(&self.work_root) {
            Ok(Some(a)) => match txn::load_record(&self.work_root.join(&a.upgrade_id)) {
                Loaded::V2(r)
                    if r.step != Step::Preparing && r.binding_sha256 == a.binding_sha256 =>
                {
                    if changed && r.step == Step::Prepared {
                        bad.push("系統已變更，紀錄卻還是 prepared".to_string());
                    }
                }
                other => bad.push(format!("指標存在但沒有屬於它的可用紀錄：{other:?}")),
            },
            // 交易結束時最後才清除指標，之後沒有 checkpoint
            Ok(None) => {
                if changed {
                    bad.push("系統已變更但沒有指標".to_string());
                }
            }
            Err(e) => bad.push(format!("指標不可用：{e}")),
        }
        for b in bad {
            self.violations.borrow_mut().push(format!("{at}: {b}"));
        }
    }

    fn simulate(&self) {
        let mut s = self.svc.borrow_mut();
        if s.active != "active" {
            return;
        }
        let v = s.running.clone();
        let b = self
            .behavior
            .borrow()
            .get(&v)
            .copied()
            .unwrap_or(Behavior::Healthy);
        let now = self.now_unix();
        let write = |registered: bool, pid: u32| {
            let f = crate::status_file::StatusFile {
                schema: crate::status_file::SCHEMA.into(),
                pid,
                version: v.clone(),
                running: true,
                connection: crate::status_file::Connection {
                    state: if registered {
                        "registered"
                    } else {
                        "reconnecting"
                    }
                    .into(),
                    ..Default::default()
                },
                reporting: crate::status_file::Reporting {
                    last_ack_at: registered.then(|| {
                        crate::status_file::ts(chrono::DateTime::from_timestamp(now, 0).unwrap())
                    }),
                    ..Default::default()
                },
                ..Default::default()
            };
            std::fs::create_dir_all(self.status.parent().unwrap()).unwrap();
            std::fs::write(&self.status, serde_json::to_vec(&f).unwrap()).unwrap();
        };
        match b {
            Behavior::Healthy => write(true, s.pid),
            Behavior::NoConnect => write(false, s.pid),
            Behavior::Crash => {
                if self.mono.get().saturating_sub(s.restarted_at) >= Duration::from_secs(1) {
                    // Restart=always：systemd 以目前的執行檔再啟動一次
                    s.n_restarts += 1;
                    s.pid += 1;
                    s.restarted_at = self.mono.get();
                    s.running = binary_version(&self.install.join("pulse"));
                }
            }
            Behavior::Exit(code) => {
                s.active = "failed".into();
                s.exit = Some(code);
            }
        }
    }
}

impl System for Fake {
    fn mono(&self) -> Duration {
        self.mono.get()
    }

    fn now_unix(&self) -> i64 {
        self.t0 + i64::try_from(self.mono.get().as_secs()).unwrap()
    }

    fn sleep(&self, d: Duration) {
        self.mono.set(self.mono.get() + d);
        self.simulate();
        // 健康確認、回滾確認的等待中（對照表第 14、19 列；只在第一次執行時注入）
        match self.restarts() {
            1 => self.maybe_crash("health_wait"),
            2 => self.maybe_crash("rollback_wait"),
            _ => {}
        }
    }

    fn restart_service(&self) -> Result<(), String> {
        self.calls.borrow_mut().push("restart".into());
        if let Some(f) = self.on_restart.borrow().as_ref() {
            f();
        }
        {
            let mut s = self.svc.borrow_mut();
            s.active = "active".into();
            s.exit = None;
            s.pid += 1;
            s.restarted_at = self.mono.get();
            s.running = binary_version(&self.install.join("pulse"));
        }
        Ok(())
    }

    fn service_state(&self) -> Result<ServiceState, String> {
        self.simulate();
        let s = self.svc.borrow();
        Ok(ServiceState {
            active: s.active.clone(),
            n_restarts: s.n_restarts,
            main_pid: s.pid,
            exit_status: s.exit,
        })
    }

    fn daemon_reload(&self) -> Result<(), String> {
        self.calls.borrow_mut().push("daemon-reload".into());
        Ok(())
    }

    fn boot_id(&self) -> Option<String> {
        (!self.no_boot_id.get()).then(|| format!("boot-{}", self.boot))
    }

    fn checkpoint(&self, name: &str) {
        self.seen.borrow_mut().push(name.to_string());
        self.observe(name);
        if let Some((at, f)) = self.at_checkpoint.borrow().as_ref()
            && at == name
        {
            f();
        }
        self.maybe_crash(name);
    }

    fn run(&self, cmd: &Cmd) -> Output {
        let ok = |stdout: String| Output {
            status: Some(0),
            stdout,
            stderr: String::new(),
        };
        match cmd {
            Cmd::TarExtract { archive, dir } => {
                self.calls.borrow_mut().push("tar".into());
                let top = dir.join("pulse-new-linux-x86_64");
                std::fs::create_dir_all(&top).unwrap();
                std::fs::copy(archive, top.join("pulse")).unwrap();
                std::fs::write(top.join("pulse.service"), NEW_UNIT).unwrap();
                ok(String::new())
            }
            Cmd::DebExtract { package, dir } | Cmd::RpmExtract { package, dir } => {
                self.calls.borrow_mut().push("extract".into());
                std::fs::create_dir_all(dir.join("opt/pulse")).unwrap();
                std::fs::copy(package, dir.join("opt/pulse/pulse")).unwrap();
                ok(String::new())
            }
            Cmd::DpkgInstall { package } | Cmd::RpmInstall { package, .. } => {
                let name = if matches!(cmd, Cmd::DpkgInstall { .. }) {
                    "dpkg"
                } else {
                    "rpm"
                };
                let file = package.file_name().unwrap().to_string_lossy().to_string();
                self.calls.borrow_mut().push(format!("{name} {file}"));
                if self.pkg_fails.get() {
                    return Output {
                        status: Some(1),
                        stdout: String::new(),
                        stderr: "dpkg: error processing archive (simulated failure)\n".into(),
                    };
                }
                if self.dpkg_locks.get() > 0 {
                    self.dpkg_locks.set(self.dpkg_locks.get() - 1);
                    return Output {
                        status: Some(2),
                        stdout: String::new(),
                        stderr: "dpkg: error: dpkg frontend lock was locked by another process with pid 7\n".into(),
                    };
                }
                let version = binary_version(package);
                // 安裝到一半就被終止（對照表第 10、16 列）：執行檔已換、套件資料庫是半安裝
                let point = if file == txn::ROLLBACK_PKG_FILE {
                    "rollback_package_running"
                } else {
                    "package_running"
                };
                if self.crash_at.borrow().as_deref() == Some(point) {
                    std::fs::copy(package, self.install.join("pulse")).unwrap();
                    *self.pkg_db.borrow_mut() = Some(format!("{version} iF"));
                    self.seen.borrow_mut().push(point.to_string());
                    self.observe(point);
                    self.maybe_crash(point);
                }
                std::fs::copy(package, self.install.join("pulse")).unwrap();
                *self.pkg_db.borrow_mut() = Some(format!("{version} ii"));
                ok(String::new())
            }
            Cmd::PackageVersion { .. } => match self.pkg_db.borrow().as_ref() {
                Some(v) => ok(v.clone()),
                None => Output {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "package pulse is not installed\n".into(),
                },
            },
            Cmd::RunAsService { program, args, .. } => {
                if args.first().map(String::as_str) == Some("--version") {
                    let v = self
                        .version_override
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| binary_version(program));
                    ok(format!("pulse {v} (5e6f7a8)\n"))
                } else {
                    if let Some(f) = self.after_precheck.borrow().as_ref() {
                        f();
                    }
                    Output {
                        status: Some(self.check_config.get()),
                        stdout: "{}".into(),
                        stderr: String::new(),
                    }
                }
            }
        }
    }
}

pub(super) struct Env {
    pub(super) root: PathBuf,
    pub(super) paths: Paths,
    pub(super) key: TestKey,
}

impl Env {
    pub(super) fn staging(&self) -> PathBuf {
        self.root.join("var/lib/pulse/upgrade").join(ID)
    }

    /// root 的工作目錄（`ID` 的交易）
    pub(super) fn work(&self) -> PathBuf {
        self.paths.work_root.join(ID)
    }

    pub(super) fn installed(&self) -> String {
        binary_version(&self.paths.install_dir.join("pulse"))
    }

    /// `pulse.prev` 的版本（不存在時為空字串）
    pub(super) fn prev(&self) -> String {
        binary_version(&self.paths.install_dir.join("pulse.prev"))
    }

    pub(super) fn unit(&self) -> String {
        std::fs::read_to_string(&self.paths.unit_path).unwrap()
    }

    pub(super) fn result(&self) -> UpgradeResult {
        UpgradeResult::read(&self.root.join("var/lib/pulse/upgrade/result.json"))
            .unwrap()
            .unwrap()
    }

    pub(super) fn has_result(&self) -> bool {
        self.root.join("var/lib/pulse/upgrade/result.json").exists()
    }

    /// `ID` 的交易紀錄（必須是可用的 v2）
    pub(super) fn record(&self) -> Record {
        match txn::load_record(&self.work()) {
            Loaded::V2(r) => *r,
            other => panic!("預期可用的交易紀錄，實際 {other:?}"),
        }
    }

    pub(super) fn active(&self) -> bool {
        self.paths.work_root.join(ACTIVE_FILE).exists()
    }

    /// 建立 root 的工作目錄（根目錄與 `ID` 的；0700，與 systemd 的 `StateDirectoryMode` 相同）
    pub(super) fn make_work(&self) {
        std::fs::create_dir_all(self.work()).unwrap();
        chmod(&self.paths.work_root, 0o700);
        chmod(&self.work(), 0o700);
    }

    /// 重新開機後執行的輔助程式就是目前安裝的執行檔
    pub(super) fn helper_is_installed(&mut self) {
        self.paths.current_version = self.installed();
    }

    pub(super) fn cleanup(&self) {
        // 測試可能把目錄改成唯讀
        for d in [self.paths.work_root.clone(), self.work()] {
            if d.is_dir() && !d.is_symlink() {
                let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn artifact_name(version: &str, commit: &str, method: &str) -> String {
    let ext = match method {
        "tar" => "tar.gz",
        other => other,
    };
    format!("pulse-{version}-{commit}-linux-x86_64.{ext}")
}

/// 在暫存區放好 `from` → `to` 的升級（有官方簽章的發佈檔、清單）並寫入 `request.json`
pub(super) fn stage_upgrade(env: &Env, id: &str, from: &str, to: &str) -> Request {
    let method = env.paths.method.clone();
    let artifact = artifact_name(to, "5e6f7a8", &method);
    let content = format!("VERSION={to}\n");
    let rel = testsign::release(
        &env.key,
        to,
        "5e6f7a8",
        &[(artifact.as_str(), content.as_bytes())],
        Some((&sha(NEW_UNIT.as_bytes()), &[&sha(OLD_UNIT.as_bytes())])),
    );
    let staging = env.root.join("var/lib/pulse/upgrade").join(id);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join(&artifact), content).unwrap();
    std::fs::write(staging.join(files::SUMS_FILE), &rel.sums).unwrap();
    std::fs::write(staging.join(files::SIG_FILE), &rel.sig).unwrap();
    std::fs::write(staging.join(&rel.manifest_name), &rel.manifest).unwrap();
    let req = Request {
        schema: files::REQUEST_SCHEMA.into(),
        upgrade_id: id.into(),
        target: kairos_proto::mgmt::UpgradeTarget {
            version: to.into(),
            commit: "5e6f7a8".into(),
        },
        method,
        artifact,
        manifest: Some(rel.manifest_name.clone()),
        rollback_artifact: None,
        health_timeout_secs: 600,
        allow_downgrade: false,
        from_version: from.into(),
        requested_at: crate::protocol::now_ts(),
    };
    std::fs::write(&env.paths.request, serde_json::to_vec(&req).unwrap()).unwrap();
    req
}

/// 建立 0.9.0 已安裝、暫存了 0.9.1 的環境（`method`：tar／deb／rpm）
pub(super) fn setup(name: &str, method: &str) -> Env {
    let root = std::env::temp_dir().join(format!("pulse-helper-{name}-{}", std::process::id()));
    if root.exists() {
        // 上一次執行留下的（可能有唯讀的目錄）
        for d in [
            "var/lib/pulse-upgrade",
            &format!("var/lib/pulse-upgrade/{ID}"),
        ] {
            let d = root.join(d);
            if d.is_dir() && !d.is_symlink() {
                let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
            }
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
    let install = root.join("opt/pulse");
    let anchor = root.join("etc/kairos-pulse/allowed_signers");
    let unit = root.join("etc/systemd/system/pulse.service");
    for d in [
        install.as_path(),
        anchor.parent().unwrap(),
        unit.parent().unwrap(),
        &root.join("var/lib/pulse/upgrade"),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    // 路徑檢查（第 17.9.7 節）要求上層目錄不可由群組或其他人寫入：不依賴執行測試的帳號的 umask
    for d in ["", "opt", "opt/pulse", "var", "var/lib", "etc"] {
        chmod(&root.join(d), 0o755);
    }
    std::fs::write(install.join("pulse"), "VERSION=0.9.0\n").unwrap();
    chmod(&install.join("pulse"), 0o755);
    let key = TestKey::generate();
    std::fs::write(&anchor, key.allowed_signers()).unwrap();
    chmod(&anchor, 0o644);
    std::fs::write(
        install.join("pulse.toml"),
        format!("[upgrade]\nallowed_signers = \"{}\"\n", anchor.display()),
    )
    .unwrap();
    std::fs::write(&unit, OLD_UNIT).unwrap();
    let paths = Paths {
        request: root.join("var/lib/pulse/upgrade/request.json"),
        install_dir: install,
        work_root: root.join("var/lib/pulse-upgrade"),
        unit_path: unit,
        require_root_owner: false,
        current_version: "0.9.0".into(),
        current_commit: "1a2b3c4".into(),
        method: method.into(),
        arch: "x86_64".into(),
    };
    let env = Env { root, paths, key };
    stage_upgrade(&env, ID, "0.9.0", "0.9.1");
    env
}

pub(super) fn rewrite_request(env: &Env, f: impl FnOnce(&mut Request)) {
    let mut req: Request =
        serde_json::from_slice(&std::fs::read(&env.paths.request).unwrap()).unwrap();
    f(&mut req);
    std::fs::write(&env.paths.request, serde_json::to_vec(&req).unwrap()).unwrap();
}

/// 在暫存區放一個有官方簽章的回滾檔（`version` 的套件）並寫進請求
pub(super) fn stage_rollback(env: &Env, version: &str) -> String {
    let name = artifact_name(version, "1a2b3c4", &env.paths.method);
    let content = format!("VERSION={version}\n");
    let rel = testsign::release(
        &env.key,
        version,
        "1a2b3c4",
        &[(name.as_str(), content.as_bytes())],
        None,
    );
    std::fs::write(env.staging().join(&name), content).unwrap();
    std::fs::write(env.staging().join(files::ROLLBACK_SUMS_FILE), &rel.sums).unwrap();
    std::fs::write(env.staging().join(files::ROLLBACK_SIG_FILE), &rel.sig).unwrap();
    let n = name.clone();
    rewrite_request(env, |r| r.rollback_artifact = Some(n));
    name
}

/// 執行 [`apply`]，在 `point` 中止（模擬行程在那個 checkpoint 之後被終止）。沒有到達那個點就是測試的錯誤
pub(super) fn apply_until(paths: &Paths, fake: &Fake, point: &str) {
    *fake.crash_at.borrow_mut() = Some(point.to_string());
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(paths, fake)));
    match r {
        Err(payload) => match payload.downcast::<Crashed>() {
            Ok(c) => assert_eq!(c.0, point),
            Err(other) => std::panic::resume_unwind(other),
        },
        Ok(r) => panic!(
            "沒有到達當機點 {point}（經過 {:?}）：{r:?}",
            fake.seen.borrow()
        ),
    }
}

#[test]
fn tar_upgrade_replaces_binary_and_unmodified_unit_then_confirms_health() {
    let env = setup("tar-ok", "tar");
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert_eq!(r.code.as_deref(), Some(codes::I_UPGRADE_COMPLETED));
    assert_eq!(env.installed(), "0.9.1");
    assert_eq!(env.prev(), "0.9.0");
    assert_eq!(env.unit(), NEW_UNIT);
    let calls = fake.calls();
    assert_eq!(calls, ["tar", "daemon-reload", "restart"], "{calls:?}");
    // 結果、清理、紀錄
    assert_eq!(env.result(), r);
    assert!(!env.paths.request.exists());
    assert!(!env.staging().exists());
    assert!(
        !env.paths
            .install_dir
            .join(format!(".upgrade-{ID}"))
            .exists()
    );
    let work = env.work();
    assert!(work.join("upgrade.log").is_file());
    assert_eq!(
        std::fs::metadata(&work).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(work.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    // 交易紀錄（第 17.9.2 節）：綁定的欄位都已記錄，結束後只留紀錄與日誌，指標已清除
    let rec = env.record();
    assert_eq!(rec.schema, txn::RECORD_SCHEMA);
    assert_eq!(rec.step, Step::Done);
    assert_eq!(rec.upgrade_id, ID);
    assert_eq!(
        (rec.from_version.as_str(), rec.from_commit.as_str()),
        ("0.9.0", "1a2b3c4")
    );
    assert_eq!(rec.target.version, "0.9.1");
    assert_eq!(rec.method, "tar");
    assert_eq!(rec.old_binary_sha256, sha(b"VERSION=0.9.0\n"));
    assert_eq!(rec.new_binary_sha256, sha(b"VERSION=0.9.1\n"));
    let artifact = rec.artifact.clone().unwrap();
    assert_eq!(artifact.sha256, sha(b"VERSION=0.9.1\n"));
    assert!(!artifact.signer.is_empty(), "{artifact:?}");
    assert_eq!(
        rec.unit,
        Some(UnitPlan {
            old_sha256: sha(OLD_UNIT.as_bytes()),
            new_sha256: sha(NEW_UNIT.as_bytes()),
        })
    );
    assert!(rec.prev_saved && rec.unit_backed_up && rec.unit_replaced && rec.service_restarted);
    assert_eq!(rec.pkg_db_before, None);
    assert_eq!(rec.binding_sha256, rec.compute_binding());
    assert_eq!(rec.health_timeout_secs, 600);
    assert_eq!(rec.result.as_ref(), Some(&r));
    assert_eq!(txn::traces(&work), Vec::<String>::new());
    assert!(!env.active());
    // 健康確認需要穩定 60 秒
    assert!(fake.mono() >= STABLE_FOR, "{:?}", fake.mono());
    fake.assert_invariants();
    env.cleanup();
}

#[test]
fn modified_unit_is_left_alone() {
    let env = setup("unit", "tar");
    std::fs::write(&env.paths.unit_path, "[Unit]\n# 客戶自己改過\n").unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded");
    assert_eq!(r.detail, "unit_not_updated");
    assert!(env.unit().contains("客戶自己改過"));
    assert!(!fake.calls().contains(&"daemon-reload".to_string()));
    assert_eq!(env.record().unit, None);
    fake.assert_invariants();
    env.cleanup();
}

#[test]
fn crash_after_restart_rolls_back_binary_and_unit() {
    let env = setup("crash", "tar");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert_eq!(r.code.as_deref(), Some(codes::W_UPGRADE_ROLLED_BACK));
    assert!(r.detail.contains("crash_restart"), "{}", r.detail);
    assert_eq!(env.installed(), "0.9.0");
    assert_eq!(env.unit(), OLD_UNIT);
    assert_eq!(fake.calls().iter().filter(|c| *c == "restart").count(), 2);
    assert!(!env.staging().exists());
    // 回滾的意圖在還原之前就記下（原因、類別、方式）
    let rec = env.record();
    let intent = rec.rollback_intent.clone().unwrap();
    assert!(intent.reason.starts_with("crash_restart"), "{intent:?}");
    assert_eq!((intent.cause, intent.via), (Cause::Health, Via::Binary));
    assert!(!env.active());
    fake.assert_invariants();
    env.cleanup();
}

#[test]
fn health_timeout_and_exit_codes_roll_back() {
    let env = setup("timeout", "tar");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::NoConnect);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back");
    assert!(r.detail.contains("health_timeout"), "{}", r.detail);
    assert!(r.detail.contains("not_registered"), "{}", r.detail);
    assert!(fake.mono() >= Duration::from_secs(600));
    env.cleanup();

    let env = setup("exit3", "tar");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Exit(3));
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back");
    assert!(r.detail.starts_with("exit_code_3"), "{}", r.detail);
    assert!(
        fake.mono() < Duration::from_secs(600),
        "結束代碼 3 立即回滾"
    );
    env.cleanup();
}

#[test]
fn rollback_that_does_not_recover_needs_manual_attention() {
    let env = setup("rbfail", "tar");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    fake.set("0.9.0", Behavior::NoConnect);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rollback_failed", "{r:?}");
    assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_ROLLBACK_FAILED));
    assert!(env.staging().exists(), "保留暫存供人工處理");
    assert!(!env.paths.request.exists(), "不再觸發");
    assert_eq!(env.result().state, "rollback_failed");
    // 最近一筆 rollback_failed 保留工作目錄的全部檔案；交易已結束，指標清除
    assert!(env.work().join(txn::NEW_BINARY_FILE).is_file());
    assert_eq!(env.record().step, Step::Done);
    assert!(!env.active());
    env.cleanup();
}

#[test]
fn precheck_failures_change_nothing() {
    let env = setup("precheck-version", "tar");
    let fake = Fake::new(&env.root);
    *fake.version_override.borrow_mut() = Some("0.9.9".into());
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_PRECHECK))
    );
    assert_eq!(env.installed(), "0.9.0");
    assert!(!fake.calls().contains(&"restart".to_string()));
    assert!(!env.paths.install_dir.join("pulse.prev").exists());
    // 沒有動手：沒有指標；結果記在紀錄裡，預檢用的目錄已刪除
    assert!(!env.active());
    assert_eq!(env.record().result.as_ref(), Some(&r));
    assert!(
        !env.paths
            .install_dir
            .join(format!(".upgrade-{ID}"))
            .exists()
    );
    fake.assert_invariants();
    env.cleanup();

    let env = setup("precheck-config", "tar");
    let fake = Fake::new(&env.root);
    fake.check_config.set(3);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_PRECHECK));
    assert!(r.detail.contains("check-config"), "{}", r.detail);
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();
    // 設定檢查回 8（有警告）仍通過
    let env = setup("precheck-warn", "tar");
    let fake = Fake::new(&env.root);
    fake.check_config.set(8);
    assert_eq!(apply(&env.paths, &fake).unwrap().state, "succeeded");
    env.cleanup();
}

#[test]
fn reverification_rejects_untrusted_or_swapped_files() {
    // 信任錨中沒有這把金鑰
    let env = setup("sig", "tar");
    let other = TestKey::generate();
    std::fs::write(
        env.root.join("etc/kairos-pulse/allowed_signers"),
        other.allowed_signers(),
    )
    .unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_SIGNATURE))
    );
    assert_eq!(env.installed(), "0.9.0");
    assert!(fake.calls().is_empty());
    env.cleanup();

    // Pulse 驗證後換掉了發佈檔（TOCTOU）：雜湊不符
    let env = setup("swap", "tar");
    let req: Request = serde_json::from_slice(&std::fs::read(&env.paths.request).unwrap()).unwrap();
    std::fs::write(env.staging().join(&req.artifact), b"VERSION=evil\n").unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_INTEGRITY));
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();

    // 符號連結與硬連結
    for (i, link) in ["symlink", "hardlink"].into_iter().enumerate() {
        let env = setup(&format!("link{i}"), "tar");
        let req: Request =
            serde_json::from_slice(&std::fs::read(&env.paths.request).unwrap()).unwrap();
        let target = env.root.join("elsewhere");
        std::fs::rename(env.staging().join(&req.artifact), &target).unwrap();
        if link == "symlink" {
            std::os::unix::fs::symlink(&target, env.staging().join(&req.artifact)).unwrap();
        } else {
            std::fs::hard_link(&target, env.staging().join(&req.artifact)).unwrap();
        }
        let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
        assert_eq!(
            r.code.as_deref(),
            Some(codes::E_UPGRADE_INTEGRITY),
            "{link}: {r:?}"
        );
        env.cleanup();
    }

    // 降版：pulse.toml 不允許
    let env = setup("downgrade", "tar");
    let mut paths = env.paths.clone();
    paths.current_version = "0.9.2".into();
    rewrite_request(&env, |r| {
        r.allow_downgrade = true;
        // 服務以自己的版本填 from_version；與輔助程式的版本不符的請求另有測試
        r.from_version = "0.9.2".into();
    });
    let r = apply(&paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::W_UPGRADE_DOWNGRADE));
    env.cleanup();

    // pulse.toml 停用了遠端升級（環境變數無法開啟）
    let env = setup("disabled", "tar");
    let toml = env.paths.install_dir.join("pulse.toml");
    let text = std::fs::read_to_string(&toml)
        .unwrap()
        .replace("[upgrade]\n", "[upgrade]\nenabled = false\n");
    std::fs::write(&toml, text).unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::I_UPGRADE_DISABLED));
    env.cleanup();
}

#[test]
fn invalid_requests_are_removed_so_the_path_unit_stops() {
    let env = setup("badreq", "tar");
    std::fs::write(&env.paths.request, "{not json").unwrap();
    assert!(apply(&env.paths, &Fake::new(&env.root)).is_err());
    assert!(!env.paths.request.exists());
    let env2 = setup("pathreq", "tar");
    rewrite_request(&env2, |r| r.artifact = "../../../etc/shadow".into());
    assert!(apply(&env2.paths, &Fake::new(&env2.root)).is_err());
    assert!(!env2.paths.request.exists());
    env.cleanup();
    env2.cleanup();
}

#[test]
fn deb_retries_a_locked_database_and_restores_the_binary_without_a_trusted_rollback_package() {
    let env = setup("deb-lock", "deb");
    let fake = Fake::new(&env.root);
    fake.dpkg_locks.set(2);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    let dpkg: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("dpkg"))
        .collect();
    assert_eq!(dpkg.len(), 3, "被鎖住時重試：{dpkg:?}");
    assert!(fake.mono() >= Duration::from_secs(60), "每 30 秒重試");
    // 替換前後套件資料庫的狀態都記在交易紀錄
    let rec = env.record();
    assert_eq!(rec.pkg_db_before.as_deref(), Some("0.9.0 ii"));
    assert_eq!(rec.pkg_db_after.as_deref(), Some("0.9.1 ii"));
    assert_eq!(rec.rollback, None);
    fake.assert_invariants();
    env.cleanup();

    let env = setup("deb-rb", "deb");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back");
    assert!(
        r.detail.contains("binary_restored_package_db_shows_0.9.1"),
        "{}",
        r.detail
    );
    assert_eq!(env.installed(), "0.9.0");
    fake.assert_invariants();
    env.cleanup();
}

#[test]
fn deb_rolls_back_with_a_verified_rollback_package() {
    let env = setup("deb-pkg", "deb");
    // 回滾檔：0.9.0 的 deb，以同一把金鑰簽署的 SHA256SUMS（Pulse 保存的驗證紀錄）
    let rb_name = stage_rollback(&env, "0.9.0");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert!(!r.detail.contains("binary_restored"), "{}", r.detail);
    let calls = fake.calls();
    assert!(
        calls.contains(&"dpkg rollback.pkg".to_string()),
        "{calls:?}"
    );
    assert_eq!(env.installed(), "0.9.0");
    assert_eq!(fake.pkg_db.borrow().as_deref(), Some("0.9.0 ii"));
    // 驗證過的回滾套件與它的雜湊、簽署者記在交易紀錄
    let rec = env.record();
    let rb = rec.rollback.clone().unwrap();
    assert_eq!(
        (rb.name.as_str(), rb.sha256.as_str()),
        (rb_name.as_str(), sha(b"VERSION=0.9.0\n").as_str())
    );
    assert!(!rb.signer.is_empty());
    assert_eq!(rec.rollback_intent.unwrap().via, Via::Package);
    fake.assert_invariants();
    env.cleanup();

    // 回滾檔的簽章資料被換掉：不使用，改以 pulse.prev 回復
    let env = setup("deb-pkg-bad", "deb");
    let rb_name = stage_rollback(&env, "0.9.0");
    let other = TestKey::generate();
    let rel = testsign::release(
        &other,
        "0.9.0",
        "1a2b3c4",
        &[(rb_name.as_str(), b"VERSION=0.9.0\n")],
        None,
    );
    std::fs::write(env.staging().join(files::ROLLBACK_SUMS_FILE), &rel.sums).unwrap();
    std::fs::write(env.staging().join(files::ROLLBACK_SIG_FILE), &rel.sig).unwrap();
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back");
    assert!(r.detail.contains("binary_restored"), "{}", r.detail);
    assert!(!fake.calls().contains(&"dpkg rollback.pkg".to_string()));
    assert_eq!(env.record().rollback, None);
    env.cleanup();
}

#[test]
fn rpm_upgrade_uses_rpm() {
    let env = setup("rpm", "rpm");
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert!(
        fake.calls().iter().any(|c| c.starts_with("rpm ")),
        "{:?}",
        fake.calls()
    );
    env.cleanup();
}

#[test]
fn resumes_after_a_reboot_in_the_middle_of_the_health_check() {
    // 健康確認期間重新開機：pulse-upgrade.path 再次觸發，只做健康確認（不重新替換）
    let mut env = setup("resume", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "health_wait");
    assert_eq!(env.record().step, Step::Replaced);
    env.helper_is_installed();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert!(
        after.calls().is_empty(),
        "沒有重新替換：{:?}",
        after.calls()
    );
    after.assert_invariants();
    env.cleanup();
}

/// 0.10.1（H4；REMOTE.md 第 17.9.3 節「接續 `replaced` 時的健康確認」）：接續時健康確認的期限已過，
/// 不再直接回滾——仍有 [`RESUME_GRACE`] 做與正常流程相同的檢查，健康 → `succeeded`，不健康 → 回滾
#[test]
fn a_late_resume_checks_health_before_deciding_to_roll_back() {
    let grace = RESUME_GRACE.as_secs();
    assert_eq!(grace, STABLE_FOR.as_secs() + 60);
    // 第 14 列（健康確認中）、第 13 列（還沒重新啟動）× 重新開機／只有輔助程式被終止 × 期限已過多久
    for (i, (point, rebooted, late)) in [
        ("health_wait", true, 601),
        ("health_wait", true, 30 * 86_400),
        ("health_wait", false, 601),
        ("record_replaced", true, 601),
        ("record_replaced", false, 601),
        // 期限還沒過、但只剩幾秒：同樣至少有 RESUME_GRACE
        ("health_wait", true, 590),
        ("record_replaced", false, 595),
    ]
    .into_iter()
    .enumerate()
    {
        let label = format!("{point}／重新開機 {rebooted}／{late} 秒後");
        // 期限已過且健康 → succeeded
        let mut env = setup(&format!("late-ok-{i}"), "tar");
        let fake = Fake::new(&env.root);
        apply_until(&env.paths, &fake, point);
        let before = env.record();
        assert_eq!(before.step, Step::Replaced, "{label}");
        env.helper_is_installed();
        let after = if rebooted {
            fake.reboot_after(late)
        } else {
            fake.helper_killed_after(late)
        };
        let r = apply(&env.paths, &after).unwrap();
        assert_eq!(
            (r.state.as_str(), r.code.as_deref(), r.detail.as_str()),
            ("succeeded", Some(codes::I_UPGRADE_COMPLETED), ""),
            "{label}: {r:?}"
        );
        assert_eq!(
            (
                env.installed().as_str(),
                env.prev().as_str(),
                env.unit().as_str()
            ),
            ("0.9.1", "0.9.0", NEW_UNIT),
            "{label}"
        );
        // 沒有重新替換；還沒送出的重新啟動只在主機沒有重新開機的那一次才有意義，但一律補送（第 13 列）
        let expected: &[&str] = if point == "record_replaced" {
            &["restart"]
        } else {
            &[]
        };
        assert_eq!(after.calls(), expected, "{label}");
        // 健康條件照舊：至少穩定 STABLE_FOR 才算成功，而且不超過確認窗
        let took = after.mono().as_secs();
        assert!(
            (STABLE_FOR.as_secs()..=grace + 2).contains(&took),
            "{label}: 健康確認花了 {took} 秒"
        );
        assert!(!env.active(), "{label}");
        let rec = env.record();
        assert_eq!(rec.step, Step::Done, "{label}");
        after.assert_invariants();
        env.cleanup();

        // 期限已過且不健康（連不上 Kairos）→ 確認窗結束後回滾
        let mut env = setup(&format!("late-bad-{i}"), "tar");
        let fake = Fake::new(&env.root);
        apply_until(&env.paths, &fake, point);
        env.helper_is_installed();
        let after = if rebooted {
            fake.reboot_after(late)
        } else {
            fake.helper_killed_after(late)
        };
        after.set("0.9.1", Behavior::NoConnect);
        let r = apply(&env.paths, &after).unwrap();
        assert_eq!(
            (r.state.as_str(), r.code.as_deref()),
            ("rolled_back", Some(codes::W_UPGRADE_ROLLED_BACK)),
            "{label}: {r:?}"
        );
        assert!(
            r.detail.starts_with("health_timeout (not_registered"),
            "{label}: {}",
            r.detail
        );
        assert_eq!(
            (
                env.installed().as_str(),
                env.prev().as_str(),
                env.unit().as_str()
            ),
            ("0.9.0", "0.9.0", OLD_UNIT),
            "{label}"
        );
        // 確認窗恰好是 RESUME_GRACE（不是 0，也不是整個 health_timeout 600 秒），之後確認舊版恢復
        //（穩定 STABLE_FOR）
        let took = after.mono().as_secs();
        let expected = grace + STABLE_FOR.as_secs();
        assert!(
            (expected..=expected + 10).contains(&took),
            "{label}: 確認窗與回滾確認共 {took} 秒"
        );
        after.assert_invariants();
        env.cleanup();

        // 期限已過且新版在崩潰 → 回滾（crash_restart）
        let mut env = setup(&format!("late-crash-{i}"), "tar");
        let fake = Fake::new(&env.root);
        apply_until(&env.paths, &fake, point);
        env.helper_is_installed();
        let after = if rebooted {
            fake.reboot_after(late)
        } else {
            fake.helper_killed_after(late)
        };
        after.set("0.9.1", Behavior::Crash);
        let r = apply(&env.paths, &after).unwrap();
        assert_eq!(r.state, "rolled_back", "{label}: {r:?}");
        assert!(
            r.detail.starts_with("crash_restart"),
            "{label}: {}",
            r.detail
        );
        assert_eq!(env.installed(), "0.9.0", "{label}");
        env.cleanup();
    }

    // 紀錄的期限與基準不改寫（每次接續重新計算）：確認窗進行中再被中斷，下次接續同樣有 RESUME_GRACE
    let mut env = setup("late-twice", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "record_replaced");
    let replaced = env.record();
    assert!(replaced.boot_id.is_some() && !replaced.service_restarted);
    env.helper_is_installed();
    let second = fake.helper_killed_after(601);
    // 第二次執行：補送重新啟動之後，在確認窗的等待中被中止
    apply_until(&env.paths, &second, "health_wait");
    assert_eq!(second.calls(), ["restart"]);
    let rec = env.record();
    assert_eq!(
        (
            rec.step,
            rec.service_restarted,
            rec.deadline_unix,
            rec.n_restarts_before,
            rec.boot_id.clone()
        ),
        (
            Step::Replaced,
            true,
            replaced.deadline_unix,
            replaced.n_restarts_before,
            replaced.boot_id.clone()
        ),
        "接續只記下重新啟動已送出，不改寫期限與基準"
    );
    let third = second.reboot_after(3600);
    let r = apply(&env.paths, &third).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert!(third.calls().is_empty(), "{:?}", third.calls());
    assert_eq!(env.installed(), "0.9.1");
    third.assert_invariants();
    env.cleanup();
}

/// 0.10.1（H4）：接續 `replaced` 時崩潰偵測的基準。`n_restarts_before` 是替換前量的，`NRestarts` 在重新
/// 開機後從 0 起算：先判斷接續之前有沒有崩潰重啟過，沒有證據時才決定之後的基準
#[test]
fn resuming_rebases_crash_detection_without_forgetting_earlier_crashes() {
    // 準備：這次開機中服務在替換之前已經自動重啟過 3 次（n_restarts_before = 3），健康確認中被中斷
    let prepare = |name: &str, boot_id: bool| {
        let env = setup(name, "tar");
        let fake = Fake::new(&env.root);
        fake.no_boot_id.set(!boot_id);
        fake.set_n_restarts(3);
        apply_until(&env.paths, &fake, "health_wait");
        let rec = env.record();
        assert_eq!(rec.n_restarts_before, 3, "{name}");
        assert_eq!(rec.boot_id.is_some(), boot_id, "{name}");
        (env, fake)
    };
    // 預期的 detail：接續之前的崩潰（同一次開機、重新開機後、計數被重設後）、確認窗內的崩潰
    const C5: &str = "crash_restart (NRestarts 3 → 5)";
    const A2: &str = "crash_restart (NRestarts 0 → 2 after reboot)";
    const A3: &str = "crash_restart (NRestarts 0 → 3 after reboot)";
    const A1: &str = "crash_restart (NRestarts 0 → 1 after reboot)";
    const RST: &str = "crash_restart (NRestarts 3 → reset → 1)";
    const WIN: &str = "crash_restart (NRestarts 0 → 1)";
    // 601 秒後才開機（期限 600 秒已過）
    const R: Option<u64> = Some(601);
    // （名稱、紀錄有沒有 boot_id、重新開機〔幾秒後；None：只有輔助程式被終止，601 秒後〕、接續時的
    //  NRestarts〔None：不另外設定〕、新版在確認窗內崩潰、預期的 detail〔空字串：succeeded〕）
    let cases = [
        // 沒有重新開機、沒有再崩潰（cur == before）：基準不變，健康 → succeeded
        ("same", true, None, None, false, ""),
        // 沒有重新開機、接續之前新版崩潰重啟過（cur > before）：不因為重設基準而漏掉 → 回滾
        ("crashed-before", true, None, Some(5), false, C5),
        // 重新開機過（boot_id 不同）、開機後沒有自動重啟：基準改為 0，健康 → succeeded
        ("reboot", true, R, None, false, ""),
        // 重新開機過、開機後新版崩潰重啟過 2 次（比替換前的 3 次少——0.10.0 的基準看不出來）→ 回滾
        ("reboot-crashed", true, R, Some(2), false, A2),
        // 重新開機過、開機後恰好崩潰重啟過 3 次（與替換前相同）：有 boot_id 時看得出來 → 回滾
        ("reboot-same-count", true, R, Some(3), false, A3),
        // 重新開機過、基準改為 0 之後確認窗內才崩潰：以新的基準發現（0.10.0 要崩潰超過 3 次才發現）
        ("reboot-then-crash", true, R, None, true, WIN),
        // 紀錄沒有 boot_id（0.10.0 的輔助程式替換、0.10.1 接續）：依計數本身——
        // 計數比替換前小而且不是 0（重設之後又自動重啟過）→ 回滾
        ("legacy-reset-crashed", false, R, Some(1), false, RST),
        // 計數是 0（重設之後沒有自動重啟）→ 基準 0，健康 → succeeded；之後才崩潰 → 以 0 為基準發現
        ("legacy-reset", false, R, None, false, ""),
        ("legacy-reset-then-crash", false, R, None, true, WIN),
        // 已知的殘餘窗口：沒有 boot_id、重新開機後恰好崩潰了與替換前相同的次數之後穩定下來——這一項
        // 看不出來（與 0.10.0 相同；REMOTE.md 第 17.9.3 節），其餘健康條件通過時是 succeeded
        ("legacy-same-count", false, R, Some(3), false, ""),
        // 沒有重新開機、root 執行過 reset-failed（計數歸零）、之後新版又自動重啟過 → 回滾
        ("reset-failed", true, None, Some(1), false, RST),
        // 期限內的接續（0.10.0 就有的情境）同樣套用：重新開機 30 秒後、開機後崩潰過 1 次（< 3）→ 回滾
        ("in-time", true, Some(30), Some(1), false, A1),
    ];
    for (name, boot_id, reboot, cur, crash, detail) in cases {
        let (mut env, fake) = prepare(&format!("rebase-{name}"), boot_id);
        let after = match reboot {
            Some(secs) => fake.reboot_after(secs),
            None => fake.helper_killed_after(601),
        };
        if let Some(n) = cur {
            after.set_n_restarts(n);
        }
        if crash {
            after.set("0.9.1", Behavior::Crash);
        }
        env.helper_is_installed();
        let r = apply(&env.paths, &after).unwrap();
        let (state, version) = if detail.is_empty() {
            ("succeeded", "0.9.1")
        } else {
            ("rolled_back", "0.9.0")
        };
        assert_eq!(r.state, state, "{name}: {r:?}");
        assert_eq!(r.detail, detail, "{name}: {r:?}");
        assert_eq!(env.installed(), version, "{name}");
        assert_eq!(env.prev(), "0.9.0", "{name}");
        after.assert_invariants();
        env.cleanup();
    }
}

#[test]
fn parses_systemctl_show() {
    let s = parse_show("ActiveState=active\nNRestarts=2\nMainPID=4242\nExecMainStatus=0\n");
    assert_eq!(
        s,
        ServiceState {
            active: "active".into(),
            n_restarts: 2,
            main_pid: 4242,
            exit_status: Some(0)
        }
    );
    assert_eq!(parse_show("garbage").active, "");
}

#[test]
fn units_point_at_the_request_file() {
    let path = include_str!("../../../../packaging/pulse-upgrade.path");
    let service = include_str!("../../../../packaging/pulse-upgrade.service");
    assert!(
        path.lines()
            .any(|l| l == format!("PathExists={DEFAULT_REQUEST}")),
        "{path}"
    );
    let exec = service
        .lines()
        .find_map(|l| l.strip_prefix("ExecStart="))
        .unwrap();
    assert_eq!(
        exec,
        format!("/opt/pulse/pulse upgrade apply --request {DEFAULT_REQUEST}")
    );
    // 不連網路、root 專用的工作目錄
    assert!(
        service
            .lines()
            .any(|l| l == "RestrictAddressFamilies=AF_UNIX")
    );
    assert!(service.lines().any(|l| l == "StateDirectory=pulse-upgrade"));
    assert!(service.lines().any(|l| l == "Type=oneshot"));
}

/// `rpm2cpio | cpio`：cpio 無法執行（Rocky 9 的最小安裝沒有 cpio）時立即回報失敗，不會因 rpm2cpio 寫滿管線而
/// 卡住（tests/systemd 實測：輔助程式卡到 TimeoutStartSec）。以 `cat` 輸出大於管線緩衝的資料代替 rpm2cpio
#[test]
fn rpm_extract_does_not_hang_when_cpio_cannot_run() {
    let dir = std::env::temp_dir().join(format!("pulse-helper-rpm-extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.rpm");
    std::fs::write(&big, vec![b'x'; 4 * 1024 * 1024]).unwrap();
    let cat = Path::new(first_existing(&["/usr/bin/cat", "/bin/cat"]));
    let run = |rpm2cpio: PathBuf, cpio: PathBuf| {
        let (tx, rx) = std::sync::mpsc::channel();
        let (big, dir) = (big.clone(), dir.clone());
        std::thread::spawn(move || {
            let _ = tx.send(rpm_extract(&rpm2cpio, &big, &cpio, &dir));
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("rpm_extract 卡住（管線寫滿）")
    };
    let o = run(cat.to_path_buf(), dir.join("no-such-cpio"));
    assert_eq!(o.status, None, "{o:?}");
    assert!(o.stderr.contains("no-such-cpio"), "{o:?}");
    // cpio 一開始就失敗（結束代碼 1）
    let falsy = PathBuf::from(first_existing(&["/usr/bin/false", "/bin/false"]));
    let o = run(cat.to_path_buf(), falsy.clone());
    assert_eq!(o.status, Some(1), "{o:?}");
    // rpm2cpio 失敗、cpio 成功：仍是失敗
    let truthy = PathBuf::from(first_existing(&["/usr/bin/true", "/bin/true"]));
    let o = run(falsy, truthy);
    assert!(!o.ok(), "{o:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// rpm 的替換與回滾都允許降版與重新安裝同一版（與 dpkg -i 相同）：沒有回滾檔時以 pulse.prev 回滾後，
/// 套件資料庫仍是失敗的那一版，重試同一版或推送較低的版本不能被 rpm 拒絕（版本政策依實際的執行檔判斷）
#[test]
fn rpm_install_allows_reinstall_and_older_package_database_versions() {
    let c = rpm_install_command(Path::new("/usr/bin/rpm"), Path::new("/w/pulse.rpm"));
    let args: Vec<_> = c
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        ["-U", "--oldpackage", "--replacepkgs", "/w/pulse.rpm"]
    );
}

/// 回滾確認期間重新開機：開機後接續回滾確認，結果的 detail 仍是原本的失敗原因（tests/systemd 實測時只剩
/// `resumed`，Kairos 與主控台看不出為什麼回滾）
#[test]
fn resuming_a_rollback_after_a_reboot_keeps_the_original_reason() {
    let env = setup("resume-rollback", "tar");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    apply_until(&env.paths, &fake, "rollback_wait");
    let rec = env.record();
    assert_eq!(rec.step, Step::RollingBack);
    assert!(
        rec.rollback_intent
            .is_some_and(|i| i.reason.starts_with("crash_restart")),
        "回滾的原因記在交易紀錄"
    );
    // 開機後執行的是已換回的舊版
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert!(
        r.detail.starts_with("crash_restart") && r.detail.contains("; resumed"),
        "{}",
        r.detail
    );
    assert!(
        after.calls().is_empty(),
        "沒有重新替換：{:?}",
        after.calls()
    );
    assert_eq!(env.installed(), "0.9.0");
    after.assert_invariants();
    env.cleanup();
}

/// `request.json` 由服務帳號寫入、不可信（REMOTE.md 第 5.1 節）：取得服務帳號的人謊報較舊的
/// `from_version`、附上那個舊版**有官方簽章**的套件當回滾檔，再讓新版的健康確認失敗——修正前輔助程式會以
/// `dpkg -i` 把主機裝回 0.8.0，繞過本機的 `allow_downgrade = false`。輔助程式以自己的版本為準：不符就
/// 拒絕，什麼都不改
#[test]
fn tampered_from_version_is_refused_before_anything_changes() {
    let env = setup("from-tamper", "deb");
    stage_rollback(&env, "0.8.0");
    rewrite_request(&env, |r| r.from_version = "0.8.0".into());
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_INTEGRITY)),
        "{r:?}"
    );
    assert!(
        r.detail.starts_with(DETAIL_FROM_VERSION_MISMATCH)
            && r.detail.contains("0.8.0")
            && r.detail.contains("0.9.0"),
        "{}",
        r.detail
    );
    // 結果回報的是實際的版本，不是請求中謊報的
    assert_eq!(r.from_version, "0.9.0");
    assert_eq!(env.installed(), "0.9.0");
    assert!(
        fake.calls().is_empty(),
        "不應解開或安裝任何東西：{:?}",
        fake.calls()
    );
    assert!(!env.paths.install_dir.join("pulse.prev").exists());
    assert!(!env.work().join("rollback.pkg").exists());
    assert!(!env.work().join("state.json").exists(), "還沒有交易紀錄");
    assert!(!env.active());
    assert_eq!(env.result(), r);
    env.cleanup();

    // 不是版本號的 from_version：請求本身不合法（刪除 request.json，path unit 不再觸發）
    let env = setup("from-garbage", "deb");
    rewrite_request(&env, |r| r.from_version = "../../etc".into());
    assert!(apply(&env.paths, &Fake::new(&env.root)).is_err());
    assert!(!env.paths.request.exists());
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();

    // 回滾檔是另一個版本的套件（有官方簽章）：不使用，回滾時改以 pulse.prev 回復
    let env = setup("from-rb-version", "deb");
    stage_rollback(&env, "0.8.0");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert!(r.detail.contains("binary_restored"), "{}", r.detail);
    assert_eq!(env.installed(), "0.9.0");
    assert!(!fake.calls().contains(&"dpkg rollback.pkg".to_string()));
    env.cleanup();
}

/// 健康確認期間重新開機、開機回來時期限已過而新版不健康（連不上 Kairos）的 deb 升級（有驗證過的回滾
/// 套件）；`tamper` 在接續之前改寫請求。0.10.1 起期限已過不再直接回滾，所以由「新版不健康」讓接續走回滾
fn resumed_deb_rollback(name: &str, tamper: impl FnOnce(&Env)) -> (Env, Fake, UpgradeResult) {
    let mut env = setup(name, "deb");
    stage_rollback(&env, "0.9.0");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "health_wait");
    assert_eq!(env.record().from_version, "0.9.0");
    tamper(&env);
    // 輔助程式現在是新版
    env.helper_is_installed();
    assert_eq!(env.paths.current_version, "0.9.1");
    let after = fake.reboot_after(601);
    after.set("0.9.1", Behavior::NoConnect);
    let r = apply(&env.paths, &after).unwrap();
    (env, after, r)
}

/// 重新開機後接續時執行檔（與輔助程式）已是新版：「替換前的版本」以第一次執行時寫進交易紀錄（只有 root
/// 可寫）的為準。期間 `request.json` 被改成較舊的 `from_version` 也不影響：回滾仍裝回紀錄的版本、以它做
/// 健康確認，結果加註
#[test]
fn resuming_after_a_reboot_uses_the_version_recorded_before_the_replacement() {
    // 請求沒有被動過：照常回滾到 0.9.0，沒有加註
    let (env, after, r) = resumed_deb_rollback("from-resume", |_| {});
    assert_eq!(
        (r.state.as_str(), r.from_version.as_str()),
        ("rolled_back", "0.9.0"),
        "{r:?}"
    );
    assert!(
        !r.detail.contains(NOTE_FROM_VERSION_IGNORED) && !r.detail.contains("request_"),
        "{}",
        r.detail
    );
    assert_eq!(env.installed(), "0.9.0");
    assert_eq!(after.calls(), ["dpkg rollback.pkg", "restart"]);
    after.assert_invariants();
    env.cleanup();

    // 請求的 from_version 被改成 0.8.0
    let (env, after, r) = resumed_deb_rollback("from-resume-tamper", |env| {
        rewrite_request(env, |r| r.from_version = "0.8.0".into());
    });
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert_eq!(r.from_version, "0.9.0", "以交易紀錄的版本為準：{r:?}");
    assert!(r.detail.contains(NOTE_FROM_VERSION_IGNORED), "{}", r.detail);
    assert_eq!(
        after.calls(),
        ["dpkg rollback.pkg", "restart"],
        "{:?}",
        after.calls()
    );
    assert_eq!(env.installed(), "0.9.0");
    assert_eq!(after.pkg_db.borrow().as_deref(), Some("0.9.0 ii"));
    env.cleanup();

    // 回滾確認期間重新開機、請求被改寫：同樣以交易紀錄的版本確認
    let env = setup("from-resume-rollback", "deb");
    stage_rollback(&env, "0.9.0");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    apply_until(&env.paths, &fake, "rollback_wait");
    rewrite_request(&env, |r| r.from_version = "0.8.0".into());
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(
        (r.state.as_str(), r.from_version.as_str()),
        ("rolled_back", "0.9.0"),
        "{r:?}"
    );
    assert!(r.detail.contains(NOTE_FROM_VERSION_IGNORED), "{}", r.detail);
    assert!(after.calls().is_empty(), "{:?}", after.calls());
    env.cleanup();
}

/// 安裝方式由輔助程式自己偵測，不採用 `request.json` 的 `method`；替換後 `method`、`target`、回滾套件
/// 一律取自替換前寫進交易紀錄的值——重新開機接續時請求被改寫也不影響回滾的方式
#[test]
fn method_and_target_come_from_the_helper_and_the_recorded_state() {
    // 第一次執行：請求的 method 與偵測到的不同就拒絕（deb 安裝的主機被要求當成 tar 替換）
    let env = setup("method-tamper", "tar");
    let mut paths = env.paths.clone();
    paths.method = "deb".into();
    let fake = Fake::new(&env.root);
    let r = apply(&paths, &fake).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_INTEGRITY)),
        "{r:?}"
    );
    assert!(
        r.detail.starts_with(DETAIL_METHOD_MISMATCH) && r.detail.contains("deb"),
        "{}",
        r.detail
    );
    assert_eq!(env.installed(), "0.9.0");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    env.cleanup();

    // 接續：deb 的升級在健康確認期間重新開機，期間請求被改成 tar、另一個目標、沒有回滾檔
    let mut env = setup("method-resume", "deb");
    let rb_name = stage_rollback(&env, "0.9.0");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "health_wait");
    let rec = env.record();
    assert_eq!(
        (
            rec.method.as_str(),
            rec.target.version.as_str(),
            rec.rollback.as_ref().map(|r| r.name.as_str())
        ),
        ("deb", "0.9.1", Some(rb_name.as_str()))
    );
    rewrite_request(&env, |r| {
        r.method = "tar".into();
        r.target.version = "0.9.9".into();
        r.rollback_artifact = None;
    });
    env.helper_is_installed();
    // 接續時不重新偵測：即使偵測結果不同（例如標記檔被刪）也以交易紀錄為準
    env.paths.method = "unknown".into();
    // 期限已過而新版不健康 → 回滾（0.10.1 起期限已過本身不再是回滾的理由）
    let after = fake.reboot_after(601);
    after.set("0.9.1", Behavior::NoConnect);
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert_eq!(
        (r.from_version.as_str(), r.to_version.as_str()),
        ("0.9.0", "0.9.1"),
        "{r:?}"
    );
    assert!(
        r.detail.contains(NOTE_REQUEST_FIELDS_IGNORED),
        "{}",
        r.detail
    );
    // 仍以套件管理員裝回（deb），不是 tar 的「換回 pulse.prev」
    assert!(!r.detail.contains("binary_restored"), "{}", r.detail);
    assert_eq!(after.calls(), ["dpkg rollback.pkg", "restart"]);
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();
}

/// 接續時只信 root 的交易紀錄，所以它必須 fail closed（REMOTE.md 第 17.8、17.9.3 節）：
/// 替換前任何一次寫不進去就不得繼續；紀錄損毀，或不見了而工作目錄裡已有交易的痕跡時，不得當成全新的
/// 請求（那樣會改信 `request.json`）
#[test]
fn the_state_file_fails_closed() {
    // 1. 替換前無法寫入指標或紀錄：不替換（root 不受目錄權限限制：容器中以 root 執行測試時無法模擬）
    if !running_as_root() {
        for (i, (point, lock_root)) in [
            // prepared 之後：指標寫不進去
            ("record_prepared", true),
            // 指標之後：replacing 寫不進去
            ("pointer_written", false),
            // pulse.prev 已保留：prev_saved 寫不進去
            ("prev_copied", false),
            // unit 已備份：unit_backed_up 寫不進去
            ("unit_backed_up", false),
        ]
        .into_iter()
        .enumerate()
        {
            let env = setup(&format!("state-unwritable{i}"), "tar");
            let fake = Fake::new(&env.root);
            let dir = if lock_root {
                env.paths.work_root.clone()
            } else {
                env.work()
            };
            let d = dir.clone();
            *fake.at_checkpoint.borrow_mut() =
                Some((point.to_string(), Box::new(move || chmod(&d, 0o500))));
            let r = apply(&env.paths, &fake).unwrap();
            chmod(&dir, 0o700);
            assert_eq!(
                (r.state.as_str(), r.code.as_deref()),
                ("failed", Some(codes::E_UPGRADE_REPLACE)),
                "{point}: {r:?}"
            );
            assert!(
                r.detail.starts_with(DETAIL_STATE_NOT_SAVED),
                "{point}: {}",
                r.detail
            );
            assert_eq!(env.installed(), "0.9.0", "{point}");
            assert_eq!(env.unit(), OLD_UNIT, "{point}");
            assert!(
                !fake.calls().contains(&"restart".to_string()),
                "{point}: {:?}",
                fake.calls()
            );
            assert!(matches!(env.prev().as_str(), "" | "0.9.0"), "{point}");
            assert_eq!(env.result(), r);
            fake.assert_invariants();
            env.cleanup();
        }
    }

    // 2. 紀錄損毀（不是合法的內容、schema 不符、讀不到）：不重新開始
    for (name, content) in [
        ("garbage", Some("{ not json")),
        (
            "schema",
            Some(
                r#"{"schema":"something/else","step":"replaced","deadline_unix":0,"n_restarts_before":0,"restarted_unix":0,"unit_replaced":false,"notes":[],"result":null}"#,
            ),
        ),
        ("unreadable", None),
    ] {
        let env = setup(&format!("state-{name}"), "tar");
        let work = env.work();
        env.make_work();
        match content {
            Some(c) => std::fs::write(work.join("state.json"), c).unwrap(),
            // 一個目錄：讀取失敗但不是「不存在」
            None => std::fs::create_dir(work.join("state.json")).unwrap(),
        }
        // 已替換成新版、pulse.prev 是舊版：重新開始的話會以新版覆寫 pulse.prev
        std::fs::write(env.paths.install_dir.join("pulse"), "VERSION=0.9.1\n").unwrap();
        std::fs::write(env.paths.install_dir.join("pulse.prev"), "VERSION=0.9.0\n").unwrap();
        let fake = Fake::new(&env.root);
        let r = apply(&env.paths, &fake).unwrap();
        assert_eq!(
            (r.state.as_str(), r.code.as_deref()),
            ("failed", Some(codes::E_UPGRADE_INTEGRITY)),
            "{name}: {r:?}"
        );
        assert!(
            r.detail.starts_with(DETAIL_STATE_UNUSABLE),
            "{name}: {}",
            r.detail
        );
        assert!(fake.calls().is_empty(), "{name}: {:?}", fake.calls());
        assert_eq!(env.prev(), "0.9.0", "{name}");
        assert!(work.join("state.json").exists(), "{name}: 現場保留");
        env.cleanup();
    }

    // 3. 紀錄不見了，但工作目錄裡有交易的痕跡
    let env = setup("state-missing", "deb");
    let work = env.work();
    env.make_work();
    std::fs::write(work.join("rollback.pkg"), "VERSION=0.8.0\n").unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_INTEGRITY), "{r:?}");
    assert!(
        r.detail.starts_with(DETAIL_STATE_UNUSABLE) && r.detail.contains("rollback.pkg"),
        "{}",
        r.detail
    );
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();

    // 對照：工作目錄已存在（上一次在預檢時中斷，只有日誌）但沒有動過手：照常重新開始
    let env = setup("state-fresh", "tar");
    let work = env.work();
    env.make_work();
    chmod(&work, 0o700);
    std::fs::write(work.join("upgrade.log"), "earlier attempt\n").unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert!(
        std::fs::read_to_string(work.join("upgrade.log"))
            .unwrap()
            .starts_with("earlier attempt\n"),
        "日誌保留"
    );
    env.cleanup();
}

/// 當機窗口：執行檔已替換、還沒記下 `replaced` 就中斷（紀錄仍是 `replacing`）。接續時依磁碟上的實況繼續，
/// **不得再覆寫 `pulse.prev`**——目前的執行檔已是新版，覆寫後回滾就換不回舊版
#[test]
fn a_crash_between_replacing_and_recording_it_keeps_the_old_backup() {
    let mut env = setup("crash-window", "tar");
    let fake = Fake::new(&env.root);
    // 新版一啟動就崩潰 → 接續後回滾
    fake.set("0.9.1", Behavior::Crash);
    apply_until(&env.paths, &fake, "binary_replaced");
    assert_eq!(
        (env.installed().as_str(), env.prev().as_str()),
        ("0.9.1", "0.9.0")
    );
    assert_eq!(env.record().step, Step::Replacing);
    // 輔助程式現在是新版
    env.helper_is_installed();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert_eq!(r.from_version, "0.9.0");
    assert_eq!(env.installed(), "0.9.0", "回滾換回的是舊版");
    assert_eq!(env.prev(), "0.9.0");
    assert!(
        !after.calls().contains(&"tar".to_string()),
        "接續時不再解開發佈檔：{:?}",
        after.calls()
    );
    after.assert_invariants();
    env.cleanup();

    // 在保留 pulse.prev 之前中斷（執行檔還是舊版）：照常保留
    let env = setup("crash-window-early", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "record_replacing");
    assert_eq!(env.prev(), "");
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert_eq!(env.prev(), "0.9.0");
    after.assert_invariants();
    env.cleanup();
}

/// 進行中交易的指標（REMOTE.md 第 17.8、17.9 節）：工作目錄不由 `request.json` 的 `upgrade_id` 決定。交易
/// 進行中（已替換、健康確認期間重新開機）服務帳號把請求換成另一個 `upgrade_id`：輔助程式仍接續原本的交易
/// ——不開新的交易、不重新替換、不覆寫 `pulse.prev`、不刪除原交易的工作目錄
#[test]
fn an_unfinished_transaction_is_resumed_even_if_the_request_names_another_upgrade() {
    let mut env = setup("active-swap", "tar");
    let active = env.paths.work_root.join(ACTIVE_FILE);
    // 替換期間指標存在（屬於這筆交易）
    let fake = Fake::new(&env.root);
    let seen = std::rc::Rc::new(RefCell::new(None));
    let (seen2, root2) = (seen.clone(), env.paths.work_root.clone());
    *fake.on_restart.borrow_mut() = Some(Box::new(move || {
        *seen2.borrow_mut() = txn::read_active(&root2).ok().flatten();
    }));
    apply_until(&env.paths, &fake, "health_wait");
    let pointer = seen.borrow().clone().expect("替換時指標已寫入");
    assert_eq!(pointer.upgrade_id, ID);
    assert_eq!(pointer.binding_sha256, env.record().binding_sha256);
    assert!(active.exists());

    // 健康確認中重新開機（期限已過，而且新版連不上 Kairos → 回滾）；請求被換成另一個 upgrade_id
    rewrite_request(&env, |r| {
        r.upgrade_id = OTHER_ID.into();
        // 對新的交易而言「誠實」的內容：目前的版本、重新安裝同一版
        r.from_version = "0.9.1".into();
    });
    env.helper_is_installed();
    let after = fake.reboot_after(601);
    after.set("0.9.1", Behavior::NoConnect);
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.upgrade_id, ID, "結果屬於原本的交易：{r:?}");
    assert_eq!(
        (
            r.state.as_str(),
            r.from_version.as_str(),
            r.to_version.as_str()
        ),
        ("rolled_back", "0.9.0", "0.9.1"),
        "{r:?}"
    );
    assert!(r.detail.contains(NOTE_UPGRADE_ID_IGNORED), "{}", r.detail);
    // 沒有重新驗證與替換（只有回滾後的重新啟動），pulse.prev 仍是舊版，回滾換回的是舊版
    assert_eq!(
        after.calls(),
        ["daemon-reload", "restart"],
        "只有回滾（還原 unit、重新啟動）：{:?}",
        after.calls()
    );
    assert_eq!(env.installed(), "0.9.0");
    assert_eq!(env.prev(), "0.9.0");
    assert!(!env.paths.work_root.join(OTHER_ID).exists());
    assert!(env.work().join("state.json").is_file());
    assert!(!active.exists(), "交易結束後清除");
    after.assert_invariants();
    env.cleanup();

    // 指標讀不到或內容不合法：不開新的交易，保留指標
    let env = setup("active-garbage", "tar");
    std::fs::create_dir_all(&env.paths.work_root).unwrap();
    chmod(&env.paths.work_root, 0o700);
    std::fs::write(env.paths.work_root.join(ACTIVE_FILE), "{ not json").unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_INTEGRITY)),
        "{r:?}"
    );
    assert!(
        r.detail.starts_with(DETAIL_POINTER_UNUSABLE),
        "{}",
        r.detail
    );
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert_eq!(env.installed(), "0.9.0");
    assert!(env.paths.work_root.join(ACTIVE_FILE).exists());
    assert!(!env.paths.request.exists());
    assert_eq!(env.result(), r);
    env.cleanup();

    // 紀錄不可用時指標保留（之後的請求同樣被擋下，直到人工檢查）
    let env = setup("active-kept", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "binary_replaced");
    std::fs::write(env.work().join("state.json"), "{ not json").unwrap();
    for _ in 0..2 {
        if !env.paths.request.exists() {
            stage_upgrade(&env, OTHER_ID, "0.9.1", "0.9.2");
        }
        let after = fake.reboot();
        let r = apply(&env.paths, &after).unwrap();
        assert!(r.detail.starts_with(DETAIL_STATE_UNUSABLE), "{}", r.detail);
        assert_eq!(r.upgrade_id, ID, "回報的是進行中的交易");
        assert!(env.paths.work_root.join(ACTIVE_FILE).exists());
        assert!(after.calls().is_empty(), "{:?}", after.calls());
        assert!(!env.paths.work_root.join(OTHER_ID).exists());
    }
    env.cleanup();
}

/// 上一次執行已寫下紀錄與指標（`prepared`）後中斷：接續時目標與發佈檔以交易紀錄為準——不重讀暫存區、
/// 不重新驗證，請求中的目標被改過也一樣
#[test]
fn a_verified_transaction_keeps_its_recorded_target() {
    let env = setup("verified-target", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "pointer_written");
    assert_eq!(env.record().step, Step::Prepared);
    rewrite_request(&env, |r| r.target.version = "0.9.9".into());
    // 暫存區（服務帳號可寫）被清空也不影響：只用 root 工作目錄裡驗證過的複本
    std::fs::remove_dir_all(env.staging()).unwrap();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(
        (r.state.as_str(), r.to_version.as_str()),
        ("succeeded", "0.9.1"),
        "{r:?}"
    );
    assert!(
        r.detail.contains(NOTE_REQUEST_FIELDS_IGNORED),
        "{}",
        r.detail
    );
    assert_eq!(env.installed(), "0.9.1");
    assert!(
        !after.calls().contains(&"tar".to_string()),
        "{:?}",
        after.calls()
    );
    after.assert_invariants();
    env.cleanup();

    // 對照：沒有紀錄時，目標與發佈檔不符的請求照常被拒
    let env = setup("verified-target-fresh", "tar");
    rewrite_request(&env, |r| r.target.version = "0.9.9".into());
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "failed", "{r:?}");
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();
}
