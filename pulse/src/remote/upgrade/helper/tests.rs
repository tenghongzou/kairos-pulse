//! 特權輔助程式的測試（REMOTE.md 第 12.1 節「輔助程式」）：以暫存目錄模擬 `/opt/pulse`、`/var/lib/pulse`、
//! `/var/lib/pulse-upgrade`，以假的 [`System`] 模擬 systemd、套件管理員與「替換後的 Pulse」（寫狀態檔、
//! 崩潰、連不上），時鐘也是假的（健康確認的 600 秒不必真的等）。

#![cfg(unix)]

use super::*;
use crate::remote::upgrade::testsign::{self, TestKey};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

const ID: &str = "9d3e7a10-4b2c-4f6e-8a1d-2c3b4a5f6e70";
const OLD_UNIT: &str = "[Unit]\nDescription=Pulse 0.9.0\n";
const NEW_UNIT: &str = "[Unit]\nDescription=Pulse 0.9.1\n";

fn sha(b: &[u8]) -> String {
    files::hex(ring::digest::digest(&ring::digest::SHA256, b).as_ref())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Healthy,
    Crash,
    NoConnect,
    Exit(i32),
}

#[derive(Debug, Default)]
struct Svc {
    active: String,
    n_restarts: u64,
    pid: u32,
    exit: Option<i32>,
    restarted_at: Duration,
}

/// 假的作業系統：執行檔的內容是 `VERSION=<版本>`，狀態檔由「目前安裝的版本」依它的行為寫出
struct Fake {
    install: PathBuf,
    status: PathBuf,
    mono: Cell<Duration>,
    t0: i64,
    behavior: RefCell<HashMap<String, Behavior>>,
    svc: RefCell<Svc>,
    calls: RefCell<Vec<String>>,
    dpkg_locks: Cell<u32>,
    check_config: Cell<i32>,
    version_override: RefCell<Option<String>>,
    /// 預檢的最後一步（check-config）執行時呼叫：模擬「預檢之後、替換之前」發生的事
    after_precheck: RefCell<Option<Box<dyn Fn()>>>,
}

fn binary_version(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .trim_start_matches("VERSION=")
        .to_string()
}

impl Fake {
    fn new(root: &Path) -> Self {
        Self {
            install: root.join("opt/pulse"),
            status: root.join("var/lib/pulse/status.json"),
            mono: Cell::new(Duration::ZERO),
            t0: chrono::Utc::now().timestamp(),
            behavior: RefCell::new(HashMap::new()),
            svc: RefCell::new(Svc {
                active: "active".into(),
                pid: 100,
                ..Default::default()
            }),
            calls: RefCell::new(Vec::new()),
            dpkg_locks: Cell::new(0),
            check_config: Cell::new(0),
            version_override: RefCell::new(None),
            after_precheck: RefCell::new(None),
        }
    }

    fn set(&self, version: &str, b: Behavior) {
        self.behavior.borrow_mut().insert(version.into(), b);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn simulate(&self) {
        let v = binary_version(&self.install.join("pulse"));
        let b = self
            .behavior
            .borrow()
            .get(&v)
            .copied()
            .unwrap_or(Behavior::Healthy);
        let mut s = self.svc.borrow_mut();
        if s.active != "active" {
            return;
        }
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
                    s.n_restarts += 1;
                    s.pid += 1;
                    s.restarted_at = self.mono.get();
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
    }

    fn restart_service(&self) -> Result<(), String> {
        self.calls.borrow_mut().push("restart".into());
        {
            let mut s = self.svc.borrow_mut();
            s.active = "active".into();
            s.exit = None;
            s.pid += 1;
            s.restarted_at = self.mono.get();
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

    fn run(&self, cmd: &Cmd) -> Output {
        let ok = |stdout: String| Output {
            status: Some(0),
            stdout,
            stderr: String::new(),
        };
        match cmd {
            Cmd::TarExtract { archive, dir } => {
                self.calls.borrow_mut().push("tar".into());
                let top = dir.join("pulse-0.9.1-5e6f7a8-linux-x86_64");
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
                self.calls.borrow_mut().push(format!(
                    "{name} {}",
                    package.file_name().unwrap().to_string_lossy()
                ));
                if self.dpkg_locks.get() > 0 {
                    self.dpkg_locks.set(self.dpkg_locks.get() - 1);
                    return Output {
                        status: Some(2),
                        stdout: String::new(),
                        stderr: "dpkg: error: dpkg frontend lock was locked by another process with pid 7\n".into(),
                    };
                }
                std::fs::copy(package, self.install.join("pulse")).unwrap();
                ok(String::new())
            }
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

struct Env {
    root: PathBuf,
    paths: Paths,
    key: TestKey,
}

impl Env {
    fn staging(&self) -> PathBuf {
        self.root.join("var/lib/pulse/upgrade").join(ID)
    }

    fn installed(&self) -> String {
        binary_version(&self.root.join("opt/pulse/pulse"))
    }

    fn result(&self) -> UpgradeResult {
        UpgradeResult::read(&self.root.join("var/lib/pulse/upgrade/result.json"))
            .unwrap()
            .unwrap()
    }
}

/// 建立 0.9.0 已安裝、暫存了 0.9.1 的環境（`method`：tar／deb／rpm）
fn setup(name: &str, method: &str) -> Env {
    let root = std::env::temp_dir().join(format!("pulse-helper-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let install = root.join("opt/pulse");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join("pulse"), "VERSION=0.9.0\n").unwrap();
    let key = TestKey::generate();
    let anchor = root.join("etc/kairos-pulse/allowed_signers");
    std::fs::create_dir_all(anchor.parent().unwrap()).unwrap();
    std::fs::write(&anchor, key.allowed_signers()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(
        install.join("pulse.toml"),
        format!("[upgrade]\nallowed_signers = \"{}\"\n", anchor.display()),
    )
    .unwrap();
    let unit = root.join("etc/systemd/system/pulse.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, OLD_UNIT).unwrap();

    let ext = match method {
        "tar" => "tar.gz",
        other => other,
    };
    let artifact = format!("pulse-0.9.1-5e6f7a8-linux-x86_64.{ext}");
    let content = b"VERSION=0.9.1\n";
    let rel = testsign::release(
        &key,
        "0.9.1",
        "5e6f7a8",
        &[(artifact.as_str(), content)],
        Some((&sha(NEW_UNIT.as_bytes()), &[&sha(OLD_UNIT.as_bytes())])),
    );
    let staging = root.join("var/lib/pulse/upgrade").join(ID);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join(&artifact), content).unwrap();
    std::fs::write(staging.join(files::SUMS_FILE), &rel.sums).unwrap();
    std::fs::write(staging.join(files::SIG_FILE), &rel.sig).unwrap();
    std::fs::write(staging.join(&rel.manifest_name), &rel.manifest).unwrap();
    let req = Request {
        schema: files::REQUEST_SCHEMA.into(),
        upgrade_id: ID.into(),
        target: kairos_proto::mgmt::UpgradeTarget {
            version: "0.9.1".into(),
            commit: "5e6f7a8".into(),
        },
        method: method.into(),
        artifact,
        manifest: Some(rel.manifest_name.clone()),
        rollback_artifact: None,
        health_timeout_secs: 600,
        allow_downgrade: false,
        from_version: "0.9.0".into(),
        requested_at: crate::protocol::now_ts(),
    };
    let request = root.join("var/lib/pulse/upgrade/request.json");
    std::fs::write(&request, serde_json::to_vec(&req).unwrap()).unwrap();
    let paths = Paths {
        request,
        install_dir: install,
        work_root: root.join("var/lib/pulse-upgrade"),
        unit_path: unit,
        require_root_owner: false,
        current_version: "0.9.0".into(),
        method: method.into(),
        arch: "x86_64".into(),
    };
    Env { root, paths, key }
}

fn rewrite_request(env: &Env, f: impl FnOnce(&mut Request)) {
    let mut req: Request =
        serde_json::from_slice(&std::fs::read(&env.paths.request).unwrap()).unwrap();
    f(&mut req);
    std::fs::write(&env.paths.request, serde_json::to_vec(&req).unwrap()).unwrap();
}

#[test]
fn tar_upgrade_replaces_binary_and_unmodified_unit_then_confirms_health() {
    let env = setup("tar-ok", "tar");
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert_eq!(r.code.as_deref(), Some(codes::I_UPGRADE_COMPLETED));
    assert_eq!(env.installed(), "0.9.1");
    assert_eq!(
        binary_version(&env.paths.install_dir.join("pulse.prev")),
        "0.9.0"
    );
    assert_eq!(
        std::fs::read_to_string(&env.paths.unit_path).unwrap(),
        NEW_UNIT
    );
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
    let work = env.paths.work_root.join(ID);
    assert!(work.join("upgrade.log").is_file());
    assert!(work.join("state.json").is_file());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&work).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // 健康確認需要穩定 60 秒
    assert!(fake.mono() >= STABLE_FOR, "{:?}", fake.mono());
    let _ = std::fs::remove_dir_all(&env.root);
}

#[test]
fn modified_unit_is_left_alone() {
    let env = setup("unit", "tar");
    std::fs::write(&env.paths.unit_path, "[Unit]\n# 客戶自己改過\n").unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded");
    assert_eq!(r.detail, "unit_not_updated");
    assert!(
        std::fs::read_to_string(&env.paths.unit_path)
            .unwrap()
            .contains("客戶自己改過")
    );
    assert!(!fake.calls().contains(&"daemon-reload".to_string()));
    let _ = std::fs::remove_dir_all(&env.root);
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
    assert_eq!(
        std::fs::read_to_string(&env.paths.unit_path).unwrap(),
        OLD_UNIT
    );
    assert_eq!(fake.calls().iter().filter(|c| *c == "restart").count(), 2);
    assert!(!env.staging().exists());
    let _ = std::fs::remove_dir_all(&env.root);
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
    let _ = std::fs::remove_dir_all(&env.root);

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
    let _ = std::fs::remove_dir_all(&env.root);
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
    let _ = std::fs::remove_dir_all(&env.root);
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
    let _ = std::fs::remove_dir_all(&env.root);

    let env = setup("precheck-config", "tar");
    let fake = Fake::new(&env.root);
    fake.check_config.set(3);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_PRECHECK));
    assert!(r.detail.contains("check-config"), "{}", r.detail);
    assert_eq!(env.installed(), "0.9.0");
    let _ = std::fs::remove_dir_all(&env.root);
    // 設定檢查回 8（有警告）仍通過
    let env = setup("precheck-warn", "tar");
    let fake = Fake::new(&env.root);
    fake.check_config.set(8);
    assert_eq!(apply(&env.paths, &fake).unwrap().state, "succeeded");
    let _ = std::fs::remove_dir_all(&env.root);
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
    let _ = std::fs::remove_dir_all(&env.root);

    // Pulse 驗證後換掉了發佈檔（TOCTOU）：雜湊不符
    let env = setup("swap", "tar");
    let req: Request = serde_json::from_slice(&std::fs::read(&env.paths.request).unwrap()).unwrap();
    std::fs::write(env.staging().join(&req.artifact), b"VERSION=evil\n").unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_INTEGRITY));
    assert_eq!(env.installed(), "0.9.0");
    let _ = std::fs::remove_dir_all(&env.root);

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
        let _ = std::fs::remove_dir_all(&env.root);
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
    let _ = std::fs::remove_dir_all(&env.root);

    // pulse.toml 停用了遠端升級（環境變數無法開啟）
    let env = setup("disabled", "tar");
    let toml = env.paths.install_dir.join("pulse.toml");
    let text = std::fs::read_to_string(&toml)
        .unwrap()
        .replace("[upgrade]\n", "[upgrade]\nenabled = false\n");
    std::fs::write(&toml, text).unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::I_UPGRADE_DISABLED));
    let _ = std::fs::remove_dir_all(&env.root);
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
    let _ = std::fs::remove_dir_all(&env.root);
    let _ = std::fs::remove_dir_all(&env2.root);
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
    let _ = std::fs::remove_dir_all(&env.root);

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
    let _ = std::fs::remove_dir_all(&env.root);
}

#[test]
fn deb_rolls_back_with_a_verified_rollback_package() {
    let env = setup("deb-pkg", "deb");
    // 回滾檔：0.9.0 的 deb，以同一把金鑰簽署的 SHA256SUMS（Pulse 保存的驗證紀錄）
    let rb_name = "pulse-0.9.0-1a2b3c4-linux-x86_64.deb";
    let rb = b"VERSION=0.9.0\n";
    let rel = testsign::release(&env.key, "0.9.0", "1a2b3c4", &[(rb_name, rb)], None);
    std::fs::write(env.staging().join(rb_name), rb).unwrap();
    std::fs::write(env.staging().join(files::ROLLBACK_SUMS_FILE), &rel.sums).unwrap();
    std::fs::write(env.staging().join(files::ROLLBACK_SIG_FILE), &rel.sig).unwrap();
    rewrite_request(&env, |r| r.rollback_artifact = Some(rb_name.into()));
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
    let _ = std::fs::remove_dir_all(&env.root);

    // 回滾檔的簽章資料被換掉：不使用，改以 pulse.prev 回復
    let env = setup("deb-pkg-bad", "deb");
    let other = TestKey::generate();
    let rel = testsign::release(&other, "0.9.0", "1a2b3c4", &[(rb_name, rb)], None);
    std::fs::write(env.staging().join(rb_name), rb).unwrap();
    std::fs::write(env.staging().join(files::ROLLBACK_SUMS_FILE), &rel.sums).unwrap();
    std::fs::write(env.staging().join(files::ROLLBACK_SIG_FILE), &rel.sig).unwrap();
    rewrite_request(&env, |r| r.rollback_artifact = Some(rb_name.into()));
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back");
    assert!(r.detail.contains("binary_restored"), "{}", r.detail);
    assert!(!fake.calls().contains(&"dpkg rollback.pkg".to_string()));
    let _ = std::fs::remove_dir_all(&env.root);
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
    let _ = std::fs::remove_dir_all(&env.root);
}

#[test]
fn resumes_after_a_reboot_in_the_middle_of_the_health_check() {
    // 第一次執行到「已替換」後中斷（模擬：先完成一次替換，再把狀態改回 Replaced、放回 request.json）
    let env = setup("resume", "tar");
    let request = std::fs::read(&env.paths.request).unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded");
    let state_path = env.paths.work_root.join(ID).join("state.json");
    let mut s: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    s["step"] = "replaced".into();
    s["result"] = serde_json::Value::Null;
    s["deadline_unix"] = (chrono::Utc::now().timestamp() + 600).into();
    s["n_restarts_before"] = 0.into();
    std::fs::write(&state_path, serde_json::to_vec(&s).unwrap()).unwrap();
    std::fs::write(&env.paths.request, &request).unwrap();
    // 重新開機後 pulse-upgrade.path 再次觸發：只做健康確認（不重新替換）
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert!(fake.calls().is_empty(), "沒有重新替換：{:?}", fake.calls());
    // 期限已過：回滾
    s["deadline_unix"] = (chrono::Utc::now().timestamp() - 1).into();
    std::fs::write(&state_path, serde_json::to_vec(&s).unwrap()).unwrap();
    std::fs::write(&env.paths.request, &request).unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert_eq!(env.installed(), "0.9.0");
    let _ = std::fs::remove_dir_all(&env.root);
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
#[cfg(unix)]
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
    let request = std::fs::read(&env.paths.request).unwrap();
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert!(r.detail.starts_with("crash_restart"), "{}", r.detail);
    // 模擬在回滾確認中途重新開機：狀態改回 rolling_back、放回 request.json
    let state_path = env.paths.work_root.join(ID).join("state.json");
    let mut s: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert!(
        s["rollback_reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("crash_restart")),
        "{s}"
    );
    s["step"] = "rolling_back".into();
    s["result"] = serde_json::Value::Null;
    s["deadline_unix"] = (chrono::Utc::now().timestamp() + 600).into();
    s["n_restarts_before"] = 0.into();
    std::fs::write(&state_path, serde_json::to_vec(&s).unwrap()).unwrap();
    std::fs::write(&env.paths.request, &request).unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert!(
        r.detail.starts_with("crash_restart") && r.detail.contains("resumed"),
        "{}",
        r.detail
    );
    assert!(fake.calls().is_empty(), "沒有重新替換：{:?}", fake.calls());
    // 0.9.0 的狀態檔沒有這個欄位：照舊只回報 resumed
    s.as_object_mut().unwrap().remove("rollback_reason");
    std::fs::write(&state_path, serde_json::to_vec(&s).unwrap()).unwrap();
    std::fs::write(&env.paths.request, &request).unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(
        (r.state.as_str(), r.detail.as_str()),
        ("rolled_back", "resumed")
    );
    let _ = std::fs::remove_dir_all(&env.root);
}

/// 在暫存區放一個有官方簽章的回滾檔（`version` 的 deb）並寫進請求
fn stage_rollback(env: &Env, version: &str) {
    let name = format!("pulse-{version}-1a2b3c4-linux-x86_64.deb");
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
    rewrite_request(env, |r| r.rollback_artifact = Some(name));
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
    assert!(!env.paths.work_root.join(ID).join("rollback.pkg").exists());
    assert_eq!(env.result(), r);
    let _ = std::fs::remove_dir_all(&env.root);

    // 不是版本號的 from_version：請求本身不合法（刪除 request.json，path unit 不再觸發）
    let env = setup("from-garbage", "deb");
    rewrite_request(&env, |r| r.from_version = "../../etc".into());
    assert!(apply(&env.paths, &Fake::new(&env.root)).is_err());
    assert!(!env.paths.request.exists());
    assert_eq!(env.installed(), "0.9.0");
    let _ = std::fs::remove_dir_all(&env.root);
}

/// 重新開機後接續時執行檔（與輔助程式）已是新版：「替換前的版本」以第一次執行時寫進狀態檔（只有 root
/// 可寫）的為準。期間 `request.json` 被改成較舊的 `from_version` 也不影響：回滾仍裝回狀態檔記下的版本、
/// 以它做健康確認，結果加註
#[test]
fn resuming_after_a_reboot_uses_the_version_recorded_before_the_replacement() {
    let mut env = setup("from-resume", "deb");
    stage_rollback(&env, "0.9.0");
    let request = std::fs::read(&env.paths.request).unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    let state_path = env.paths.work_root.join(ID).join("state.json");
    let mut s: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(s["from_version"], "0.9.0", "{s}");
    // 模擬健康確認期間重新開機、期限已過：接續後回滾。輔助程式現在是新版
    s["step"] = "replaced".into();
    s["result"] = serde_json::Value::Null;
    s["deadline_unix"] = (chrono::Utc::now().timestamp() - 1).into();
    s["n_restarts_before"] = 0.into();
    std::fs::write(&state_path, serde_json::to_vec(&s).unwrap()).unwrap();
    env.paths.current_version = "0.9.1".into();
    // 驗證過的回滾檔在 root 專用的工作目錄（升級結束時才刪除：這裡放回去，模擬升級還沒結束）
    let rollback_pkg = env.paths.work_root.join(ID).join("rollback.pkg");
    let put_rollback = || {
        std::fs::write(
            &rollback_pkg,
            "VERSION=0.9.0
",
        )
        .unwrap()
    };

    // 請求沒有被動過：照常回滾到 0.9.0，沒有加註
    put_rollback();
    std::fs::write(&env.paths.request, &request).unwrap();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(
        (r.state.as_str(), r.from_version.as_str()),
        ("rolled_back", "0.9.0"),
        "{r:?}"
    );
    assert!(
        !r.detail.contains(NOTE_FROM_VERSION_IGNORED),
        "{}",
        r.detail
    );
    assert_eq!(env.installed(), "0.9.0");

    // 回到「已替換」，這次請求的 from_version 被改成 0.8.0
    std::fs::write(env.paths.install_dir.join("pulse"), "VERSION=0.9.1\n").unwrap();
    std::fs::write(&state_path, serde_json::to_vec(&s).unwrap()).unwrap();
    std::fs::write(&env.paths.request, &request).unwrap();
    rewrite_request(&env, |r| r.from_version = "0.8.0".into());
    put_rollback();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert_eq!(r.from_version, "0.9.0", "以狀態檔的版本為準：{r:?}");
    assert!(r.detail.contains(NOTE_FROM_VERSION_IGNORED), "{}", r.detail);
    assert_eq!(
        fake.calls(),
        ["dpkg rollback.pkg", "restart"],
        "{:?}",
        fake.calls()
    );
    assert_eq!(env.installed(), "0.9.0");

    // 回滾確認期間再重新開機：同樣以狀態檔的版本確認
    let mut s2: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    s2["step"] = "rolling_back".into();
    s2["result"] = serde_json::Value::Null;
    s2["deadline_unix"] = (chrono::Utc::now().timestamp() + 600).into();
    s2["n_restarts_before"] = 0.into();
    std::fs::write(&state_path, serde_json::to_vec(&s2).unwrap()).unwrap();
    std::fs::write(&env.paths.request, &request).unwrap();
    rewrite_request(&env, |r| r.from_version = "0.8.0".into());
    env.paths.current_version = "0.9.0".into();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(
        (r.state.as_str(), r.from_version.as_str()),
        ("rolled_back", "0.9.0"),
        "{r:?}"
    );

    // 0.9.0 的輔助程式寫的狀態檔沒有記版本：接續時沿用請求中的值（與 0.9.0 相同）
    s.as_object_mut().unwrap().remove("from_version");
    std::fs::write(env.paths.install_dir.join("pulse"), "VERSION=0.9.1\n").unwrap();
    std::fs::write(&state_path, serde_json::to_vec(&s).unwrap()).unwrap();
    std::fs::write(&env.paths.request, &request).unwrap();
    env.paths.current_version = "0.9.1".into();
    put_rollback();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(
        (r.state.as_str(), r.from_version.as_str()),
        ("rolled_back", "0.9.0"),
        "{r:?}"
    );
    let _ = std::fs::remove_dir_all(&env.root);
}

/// 安裝方式由輔助程式自己偵測，不採用 `request.json` 的 `method`；替換後 `method`、`target`、
/// `rollback_artifact` 一律取自替換前寫進狀態檔的值——重新開機接續時請求被改寫也不影響回滾的方式
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
    let _ = std::fs::remove_dir_all(&env.root);

    // 接續：deb 的升級在健康確認期間重新開機，期間請求被改成 tar、另一個目標、沒有回滾檔
    let mut env = setup("method-resume", "deb");
    stage_rollback(&env, "0.9.0");
    let request = std::fs::read(&env.paths.request).unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    let state_path = env.paths.work_root.join(ID).join("state.json");
    let mut s: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(
        (
            s["method"].as_str(),
            s["target"]["version"].as_str(),
            s["rollback_artifact"].as_str()
        ),
        (
            Some("deb"),
            Some("0.9.1"),
            Some("pulse-0.9.0-1a2b3c4-linux-x86_64.deb")
        ),
        "{s}"
    );
    s["step"] = "replaced".into();
    s["result"] = serde_json::Value::Null;
    s["deadline_unix"] = (chrono::Utc::now().timestamp() - 1).into();
    s["n_restarts_before"] = 0.into();
    std::fs::write(&state_path, serde_json::to_vec(&s).unwrap()).unwrap();
    std::fs::write(
        env.paths.work_root.join(ID).join("rollback.pkg"),
        "VERSION=0.9.0\n",
    )
    .unwrap();
    std::fs::write(&env.paths.request, &request).unwrap();
    rewrite_request(&env, |r| {
        r.method = "tar".into();
        r.target.version = "0.9.9".into();
        r.rollback_artifact = None;
    });
    env.paths.current_version = "0.9.1".into();
    // 接續時不重新偵測：即使偵測結果不同（例如標記檔被刪）也以狀態檔為準
    env.paths.method = "unknown".into();
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
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
    assert_eq!(fake.calls(), ["dpkg rollback.pkg", "restart"]);
    assert_eq!(env.installed(), "0.9.0");
    let _ = std::fs::remove_dir_all(&env.root);
}

/// 接續時只信 root 的狀態檔，所以它必須 fail closed（REMOTE.md 第 17.8 節）：
/// 替換前寫不進狀態檔就不得開始替換；狀態檔損毀，或不見了而工作目錄裡已有替換階段的檔案時，不得當成
/// 全新的請求（那樣會改信 `request.json`）
#[test]
fn the_state_file_fails_closed() {
    use std::os::unix::fs::PermissionsExt;

    // 1. 替換前無法寫入狀態檔：不替換
    let env = setup("state-unwritable", "tar");
    let work = env.paths.work_root.join(ID);
    let fake = Fake::new(&env.root);
    let w = work.clone();
    *fake.after_precheck.borrow_mut() = Some(Box::new(move || {
        std::fs::set_permissions(&w, std::fs::Permissions::from_mode(0o500)).unwrap();
    }));
    let r = apply(&env.paths, &fake).unwrap();
    let _ = std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700));
    // root 不受目錄權限限制（容器中以 root 執行測試時）：這一段無法模擬，略過
    let probe = work.join("probe");
    std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o500)).unwrap();
    let is_root = std::fs::write(&probe, b"x").is_ok();
    std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700)).unwrap();
    if !is_root {
        assert_eq!(
            (r.state.as_str(), r.code.as_deref()),
            ("failed", Some(codes::E_UPGRADE_REPLACE)),
            "{r:?}"
        );
        assert!(r.detail.starts_with(DETAIL_STATE_NOT_SAVED), "{}", r.detail);
        assert_eq!(env.installed(), "0.9.0");
        assert!(!env.paths.install_dir.join("pulse.prev").exists());
        assert!(
            !fake.calls().contains(&"restart".to_string()),
            "{:?}",
            fake.calls()
        );
    }
    let _ = std::fs::remove_dir_all(&env.root);

    // 2. 狀態檔損毀（不是合法的內容、schema 不符、讀不到）：不重新開始
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
        let work = env.paths.work_root.join(ID);
        std::fs::create_dir_all(&work).unwrap();
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
        assert_eq!(
            binary_version(&env.paths.install_dir.join("pulse.prev")),
            "0.9.0",
            "{name}"
        );
        let _ = std::fs::remove_dir_all(&env.root);
    }

    // 3. 狀態檔不見了，但工作目錄裡有替換階段才會出現的檔案
    let env = setup("state-missing", "deb");
    let work = env.paths.work_root.join(ID);
    std::fs::create_dir_all(&work).unwrap();
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
    let _ = std::fs::remove_dir_all(&env.root);

    // 對照：工作目錄已存在（上一次在預檢時中斷，只有日誌）但沒有動過手：照常重新開始
    let env = setup("state-fresh", "tar");
    let work = env.paths.work_root.join(ID);
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("upgrade.log"), "earlier attempt\n").unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    let _ = std::fs::remove_dir_all(&env.root);
}

/// 當機窗口：替換完成、還沒記下 `Replaced` 就中斷（狀態仍是 `Verified`）。接續時重新驗證並再替換一次，
/// 但**不得再覆寫 `pulse.prev`**——目前的執行檔已是新版，覆寫後回滾就換不回舊版
#[test]
fn a_crash_between_replacing_and_recording_it_keeps_the_old_backup() {
    let mut env = setup("crash-window", "tar");
    // 上一次執行留下的狀態：已保留 pulse.prev（0.9.0）、執行檔已換成 0.9.1、狀態仍是 verified
    let work = env.paths.work_root.join(ID);
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(env.paths.install_dir.join("pulse"), "VERSION=0.9.1\n").unwrap();
    std::fs::write(env.paths.install_dir.join("pulse.prev"), "VERSION=0.9.0\n").unwrap();
    let state = HelperState {
        schema: HELPER_SCHEMA.to_string(),
        step: Step::Verified,
        deadline_unix: 0,
        n_restarts_before: 0,
        restarted_unix: 0,
        health_timeout_secs: 600,
        unit_replaced: false,
        notes: Vec::new(),
        rollback_reason: String::new(),
        from_version: "0.9.0".into(),
        method: "tar".into(),
        target: Some(kairos_proto::mgmt::UpgradeTarget {
            version: "0.9.1".into(),
            commit: "5e6f7a8".into(),
        }),
        rollback_artifact: None,
        prev_saved: true,
        result: None,
    };
    std::fs::write(work.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
    // 輔助程式現在是新版；新版一啟動就崩潰 → 回滾
    env.paths.current_version = "0.9.1".into();
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert_eq!(r.from_version, "0.9.0");
    assert_eq!(env.installed(), "0.9.0", "回滾換回的是舊版");
    assert_eq!(
        binary_version(&env.paths.install_dir.join("pulse.prev")),
        "0.9.0"
    );
    let _ = std::fs::remove_dir_all(&env.root);

    // 沒有記下「已保留」（在複製 pulse.prev 之前中斷，執行檔還是舊版）：照常保留
    let env = setup("crash-window-early", "tar");
    let work = env.paths.work_root.join(ID);
    std::fs::create_dir_all(&work).unwrap();
    let early = HelperState {
        prev_saved: false,
        ..state
    };
    std::fs::write(work.join("state.json"), serde_json::to_vec(&early).unwrap()).unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert_eq!(
        binary_version(&env.paths.install_dir.join("pulse.prev")),
        "0.9.0"
    );
    let _ = std::fs::remove_dir_all(&env.root);
}
