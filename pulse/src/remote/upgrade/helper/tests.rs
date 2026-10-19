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
    rewrite_request(&env, |r| r.allow_downgrade = true);
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
