//! hello 的 `install`（`crates/kairos-proto/REMOTE.md` 第 2.1、4.3、4.4.5 節）：安裝方式與遠端升級的本機狀態，
//! 以及 Pulse 與特權輔助程式共用的信任錨檢查（第 4.2.1 節）。
//!
//! 安裝方式（Linux，依序）：`/opt/pulse/.install-method`（套件的 postinst 寫入 `deb`／`rpm`）→
//! `/var/lib/dpkg/info/pulse.list` 存在 → `deb` → `rpm -q --quiet pulse` 成功 → `rpm` → 執行檔是
//! `/opt/pulse/pulse` 且工作目錄是 `/opt/pulse` → `tar`，否則 `unknown`。Windows 的遠端升級在 0.10.0，
//! 0.9.0 只回報 `msi`／`zip` 供主控台顯示，`install.upgrade` 一律 `unsupported`。
//!
//! 遠端升級的本機狀態（第一個符合的）：`disabled`（`upgrade.enabled = false`，`pulse.toml` 與環境變數任一為
//! `false`）→ `unsupported`（不是 Linux、`unknown` 安裝方式、多實例、容器、沒有 systemd）→
//! `no_trust_anchor`（`allowed_signers` 不存在、不是 root 擁有、其他人可寫、沒有可用的金鑰）→
//! `no_helper`（沒有啟用 `pulse-upgrade.path`）→ `unsafe_path`（0.10.0：安裝目錄、執行檔、
//! `/var/lib/pulse-upgrade` 或它們的上層目錄沒有通過輔助程式的路徑檢查；與輔助程式共用同一個函式，
//! 服務帳號看不到的部分略過）→ `ready`。

use std::path::{Path, PathBuf};

use kairos_proto::mgmt::{
    INSTALL_METHOD_DEB, INSTALL_METHOD_MSI, INSTALL_METHOD_RPM, INSTALL_METHOD_TAR,
    INSTALL_METHOD_UNKNOWN, INSTALL_METHOD_ZIP, INSTALL_UPGRADE_DISABLED,
    INSTALL_UPGRADE_NO_HELPER, INSTALL_UPGRADE_NO_TRUST_ANCHOR, INSTALL_UPGRADE_READY,
    INSTALL_UPGRADE_UNSAFE_PATH, INSTALL_UPGRADE_UNSUPPORTED,
};
use kairos_proto::release::{AllowedSigners, RELEASE_NAMESPACE};

/// Linux 的安裝目錄（tar／deb／rpm 相同，`pulse/packaging/INSTALL.md`）
pub const INSTALL_DIR: &str = "/opt/pulse";
/// 套件的 postinst 寫入的安裝方式（`deb`／`rpm`）
pub const INSTALL_METHOD_FILE: &str = "/opt/pulse/.install-method";
/// 輔助程式的 path unit
pub const HELPER_PATH_UNIT: &str = "pulse-upgrade.path";
/// 信任錨的大小上限（bytes）
pub const MAX_ALLOWED_SIGNERS_BYTES: u64 = 64 * 1024;
/// `install.upgrade = "unsafe_path"` 時 [`Detection::reason`] 的開頭：與輔助程式結果的 `detail`
/// （`unsafe_path: <路徑>: <原因>`）相同的寫法
pub const UNSAFE_PATH_REASON_PREFIX: &str = "unsafe_path: ";

/// 偵測的輸入（測試時以暫存目錄模擬根目錄）
#[derive(Debug, Clone)]
pub struct Probe {
    /// 根目錄（正式為 `/`）
    pub root: PathBuf,
    /// 目前的執行檔
    pub exe: Option<PathBuf>,
    /// 目前的工作目錄
    pub cwd: Option<PathBuf>,
    /// systemd 的 `STATE_DIRECTORY`
    pub state_directory: Option<PathBuf>,
    /// `rpm -q --quiet pulse` 是否成功
    pub rpm_installed: fn(&Path) -> bool,
    /// 信任錨的擁有者必須是 root（測試時以目前的使用者取代）
    pub require_root_owner: bool,
}

impl Probe {
    /// 這台主機的實際狀態
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            exe: std::env::current_exe().ok(),
            cwd: std::env::current_dir().ok(),
            state_directory: crate::status_file::state_directory(
                std::env::var_os("STATE_DIRECTORY").as_deref(),
            ),
            rpm_installed: rpm_query,
            require_root_owner: true,
        }
    }

    /// 命令列（`pulse upgrade status`、`pulse upgrade rollback`）：以執行檔所在的目錄代替目前目錄。
    /// 服務的工作目錄固定是 `/opt/pulse`（unit 的 `WorkingDirectory=`），管理者則可能在任何目錄執行
    /// `sudo /opt/pulse/pulse upgrade …`；以目前目錄判斷的話 tar 安裝會被當成 `unknown`（顯示「不支援」、
    /// 拒絕手動回滾）
    pub fn for_cli() -> Self {
        Self::system().with_cwd_from_exe()
    }

    /// 工作目錄改為執行檔所在的目錄（沒有執行檔路徑時不變）
    pub fn with_cwd_from_exe(mut self) -> Self {
        if let Some(dir) = self.exe.as_deref().and_then(Path::parent) {
            self.cwd = Some(dir.to_path_buf());
        }
        self
    }

    fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }
}

/// `rpm -q --quiet pulse`（`rpm` 不存在時 `false`）
fn rpm_query(root: &Path) -> bool {
    let rpm = ["usr/bin/rpm", "bin/rpm"]
        .iter()
        .map(|p| root.join(p))
        .find(|p| p.is_file());
    let Some(rpm) = rpm else {
        return false;
    };
    std::process::Command::new(rpm)
        .args(["-q", "--quiet", "pulse"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 安裝方式（REMOTE.md 第 4.3 節）
pub fn detect_method(p: &Probe) -> &'static str {
    if cfg!(windows) {
        return windows_method(p);
    }
    if let Ok(text) = std::fs::read_to_string(p.path(INSTALL_METHOD_FILE)) {
        match text.trim() {
            "deb" => return INSTALL_METHOD_DEB,
            "rpm" => return INSTALL_METHOD_RPM,
            _ => {}
        }
    }
    if p.path("/var/lib/dpkg/info/pulse.list").is_file() {
        return INSTALL_METHOD_DEB;
    }
    if (p.rpm_installed)(&p.root) {
        return INSTALL_METHOD_RPM;
    }
    let exe_ok = p
        .exe
        .as_deref()
        .is_some_and(|e| e == p.path("/opt/pulse/pulse"));
    let cwd_ok = p.cwd.as_deref().is_some_and(|c| c == p.path(INSTALL_DIR));
    if exe_ok && cwd_ok {
        INSTALL_METHOD_TAR
    } else {
        INSTALL_METHOD_UNKNOWN
    }
}

/// Windows：執行檔在 `%ProgramFiles%\Pulse`（MSI 的安裝位置）時 `msi`，否則 `zip`
/// （0.10.0 改以登錄的 UpgradeCode 判斷，REMOTE.md 第 4.3 節）
fn windows_method(p: &Probe) -> &'static str {
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
    match (p.exe.as_deref().and_then(Path::parent), program_files) {
        (Some(dir), Some(pf)) if dir.eq_ignore_ascii_case_path(&pf.join("Pulse")) => {
            INSTALL_METHOD_MSI
        }
        _ => INSTALL_METHOD_ZIP,
    }
}

trait PathEqIgnoreCase {
    fn eq_ignore_ascii_case_path(&self, other: &Path) -> bool;
}

impl PathEqIgnoreCase for Path {
    fn eq_ignore_ascii_case_path(&self, other: &Path) -> bool {
        self.to_string_lossy()
            .eq_ignore_ascii_case(&other.to_string_lossy())
    }
}

/// 在容器中執行（REMOTE.md 第 4.4.5 節：以映像檔管理版本，不在容器內替換）
fn in_container(p: &Probe) -> bool {
    if p.path("/.dockerenv").exists() || p.path("/run/.containerenv").exists() {
        return true;
    }
    std::fs::read_to_string(p.path("/proc/1/cgroup")).is_ok_and(|c| {
        ["docker", "kubepods", "containerd", "libpod", "/lxc/"]
            .iter()
            .any(|k| c.contains(k))
    })
}

/// 讀取並檢查信任錨（REMOTE.md 第 4.2.1 節）：一般檔案、不超過 64 KiB、（正式環境）root 擁有、
/// 群組與其他人不可寫，且至少有一把可用於 `kairos-pulse-release` 的金鑰。被拒絕的行另外回傳給呼叫端記錄
pub fn load_trust_anchor(path: &Path, require_root_owner: bool) -> Result<AllowedSigners, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{} 不存在", path.display()),
        _ => format!("無法讀取 {}：{e}", path.display()),
    })?;
    if !meta.file_type().is_file() {
        return Err(format!("{} 不是一般檔案（不接受符號連結）", path.display()));
    }
    if meta.len() > MAX_ALLOWED_SIGNERS_BYTES {
        return Err(format!(
            "{} 超過 {MAX_ALLOWED_SIGNERS_BYTES} bytes",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if require_root_owner && meta.uid() != 0 {
            return Err(format!(
                "{} 必須由 root 擁有（目前 uid {}）",
                path.display(),
                meta.uid()
            ));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{} 的權限過寬（{:o}）：群組與其他使用者不可寫",
                path.display(),
                meta.mode() & 0o777
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = require_root_owner;
    let text = crate::remote::read_small(path, MAX_ALLOWED_SIGNERS_BYTES)
        .map_err(|e| format!("無法讀取 {}：{e}", path.display()))?;
    let text =
        String::from_utf8(text).map_err(|_| format!("{} 不是 UTF-8 文字", path.display()))?;
    let signers = AllowedSigners::parse(&text);
    if !signers.has_usable_entry(RELEASE_NAMESPACE) {
        let rejected = signers
            .rejected
            .iter()
            .map(|r| format!("第 {} 行：{}", r.line, r.reason))
            .collect::<Vec<_>>();
        return Err(format!(
            "{} 沒有可用於 {RELEASE_NAMESPACE} 的 ssh-ed25519 金鑰（namespaces 必須包含 {RELEASE_NAMESPACE}）{}",
            path.display(),
            if rejected.is_empty() {
                String::new()
            } else {
                format!("；被拒絕的行：{}", rejected.join("；"))
            }
        ));
    }
    Ok(signers)
}

/// 偵測的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// `install.method`
    pub method: &'static str,
    /// `install.upgrade`
    pub upgrade: &'static str,
    /// `upgrade` 不是 `ready` 時的原因（日誌與 `pulse upgrade status`）
    pub reason: Option<String>,
    /// 信任錨的路徑
    pub trust_anchor: PathBuf,
    /// 信任錨中可用的金鑰指紋（`SHA256:…`）
    pub fingerprints: Vec<String>,
}

/// 遠端升級的本機狀態（REMOTE.md 第 2.1 節）。`enabled`：`upgrade.enabled` 在 `pulse.toml` 與環境變數都不為
/// `false`
pub fn detect(enabled: bool, trust_anchor: &Path, p: &Probe) -> Detection {
    let method = detect_method(p);
    let mut d = Detection {
        method,
        upgrade: INSTALL_UPGRADE_READY,
        reason: None,
        trust_anchor: trust_anchor.to_path_buf(),
        fingerprints: Vec::new(),
    };
    let mut set = |state: &'static str, reason: String| {
        d.upgrade = state;
        d.reason = Some(reason);
    };
    if !enabled {
        set(
            INSTALL_UPGRADE_DISABLED,
            "upgrade.enabled = false".to_string(),
        );
        return d;
    }
    if !cfg!(target_os = "linux") {
        set(
            INSTALL_UPGRADE_UNSUPPORTED,
            "0.9.0 的遠端升級只支援 Linux（Windows 在 0.10.0）".to_string(),
        );
        return d;
    }
    if method == INSTALL_METHOD_UNKNOWN {
        set(
            INSTALL_UPGRADE_UNSUPPORTED,
            "無法判斷安裝方式（不是 /opt/pulse 的 tar 安裝，也不是 deb／rpm 套件）".to_string(),
        );
        return d;
    }
    if let Some(n) = p
        .state_directory
        .as_deref()
        .and_then(crate::status_file::instance_of)
    {
        set(
            INSTALL_UPGRADE_UNSUPPORTED,
            format!("多實例（第 {n} 個實例）不支援遠端升級，請手動升級"),
        );
        return d;
    }
    if in_container(p) {
        set(
            INSTALL_UPGRADE_UNSUPPORTED,
            "在容器中執行（請以映像檔管理版本）".to_string(),
        );
        return d;
    }
    if !p.path("/run/systemd/system").is_dir() {
        set(
            INSTALL_UPGRADE_UNSUPPORTED,
            "不是以 systemd 管理的系統".to_string(),
        );
        return d;
    }
    match load_trust_anchor(trust_anchor, p.require_root_owner) {
        Ok(signers) => {
            d.fingerprints = signers
                .entries
                .iter()
                .filter(|e| {
                    e.namespaces
                        .as_ref()
                        .is_some_and(|ns| ns.iter().any(|n| n == RELEASE_NAMESPACE))
                })
                .map(|e| e.fingerprint())
                .collect();
        }
        Err(e) => {
            d.upgrade = INSTALL_UPGRADE_NO_TRUST_ANCHOR;
            d.reason = Some(e);
            return d;
        }
    }
    let helper_enabled = [
        "/etc/systemd/system/paths.target.wants",
        "/etc/systemd/system/multi-user.target.wants",
    ]
    .iter()
    .any(|dir| p.path(dir).join(HELPER_PATH_UNIT).exists());
    if !helper_enabled {
        d.upgrade = INSTALL_UPGRADE_NO_HELPER;
        d.reason = Some(format!(
            "沒有啟用 {HELPER_PATH_UNIT}（systemctl enable --now {HELPER_PATH_UNIT}）"
        ));
        return d;
    }
    // 其他條件都就緒：事先做輔助程式第一步的路徑檢查（同一個函式；REMOTE.md 第 17.9.7 節）。不通過的話
    // 推送會以 PLS-E8405（unsafe_path）失敗，所以在 hello 就回報，讓 Kairos 的資格預覽標出這台主機。
    // 服務帳號看不到的部分略過（不誤報）；輔助程式執行時仍會自己再檢查一次
    if let Err(why) = crate::remote::upgrade::helper::precheck_paths(
        &p.path(INSTALL_DIR),
        &p.path(crate::remote::upgrade::helper::WORK_ROOT),
        p.require_root_owner,
    ) {
        d.upgrade = INSTALL_UPGRADE_UNSAFE_PATH;
        d.reason = Some(format!("{UNSAFE_PATH_REASON_PREFIX}{why}"));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pulse-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn probe(root: &Path) -> Probe {
        Probe {
            root: root.to_path_buf(),
            exe: Some(root.join("opt/pulse/pulse")),
            cwd: Some(root.join("opt/pulse")),
            state_directory: None,
            rpm_installed: |_| false,
            require_root_owner: false,
        }
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[cfg(unix)]
    const SIGNERS: &str =
        include_str!("../../../crates/kairos-proto/testdata/release/allowed_signers");

    #[cfg(target_os = "linux")]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn ready_root(name: &str) -> PathBuf {
        let root = tmp(name);
        std::fs::create_dir_all(root.join("opt/pulse")).unwrap();
        std::fs::create_dir_all(root.join("var/lib")).unwrap();
        std::fs::create_dir_all(root.join("run/systemd/system")).unwrap();
        write(&root, "opt/pulse/pulse", "VERSION=test\n");
        // 路徑檢查（REMOTE.md 第 17.9.7 節）要求群組與其他人不可寫：不依賴執行測試的帳號的 umask
        for p in ["", "opt", "opt/pulse", "opt/pulse/pulse", "var", "var/lib"] {
            chmod(&root.join(p), 0o755);
        }
        write(&root, "etc/kairos-pulse/allowed_signers", SIGNERS);
        let anchor = root.join("etc/kairos-pulse/allowed_signers");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o644)).unwrap();
        write(
            &root,
            "etc/systemd/system/paths.target.wants/pulse-upgrade.path",
            "",
        );
        root
    }

    #[test]
    fn install_method_follows_the_spec_order() {
        if cfg!(windows) {
            return;
        }
        let root = tmp("method");
        let mut p = probe(&root);
        assert_eq!(detect_method(&p), INSTALL_METHOD_TAR);
        p.cwd = Some(root.join("srv"));
        assert_eq!(
            detect_method(&p),
            INSTALL_METHOD_UNKNOWN,
            "工作目錄不是 /opt/pulse"
        );
        p.cwd = Some(root.join("opt/pulse"));
        p.exe = Some(root.join("usr/local/bin/pulse"));
        assert_eq!(detect_method(&p), INSTALL_METHOD_UNKNOWN);
        p.rpm_installed = |_| true;
        assert_eq!(detect_method(&p), INSTALL_METHOD_RPM);
        write(&root, "var/lib/dpkg/info/pulse.list", "/opt/pulse/pulse\n");
        assert_eq!(detect_method(&p), INSTALL_METHOD_DEB);
        write(&root, "opt/pulse/.install-method", "rpm\n");
        assert_eq!(
            detect_method(&p),
            INSTALL_METHOD_RPM,
            ".install-method 優先"
        );
        write(&root, "opt/pulse/.install-method", "garbage\n");
        assert_eq!(detect_method(&p), INSTALL_METHOD_DEB);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 命令列在任何目錄執行（例如 `sudo /opt/pulse/pulse upgrade rollback --yes` 從家目錄）都要判斷為 tar：
    /// 以執行檔所在的目錄代替目前目錄（tests/systemd 實測發現 tar 主機的 `pulse upgrade status` 顯示 unknown）
    #[test]
    fn cli_probe_uses_the_executable_directory_as_working_directory() {
        if cfg!(windows) {
            return;
        }
        let root = tmp("cli-cwd");
        let mut p = probe(&root);
        p.cwd = Some(PathBuf::from("/"));
        assert_eq!(detect_method(&p), INSTALL_METHOD_UNKNOWN);
        let p = p.with_cwd_from_exe();
        assert_eq!(p.cwd.as_deref(), Some(root.join("opt/pulse").as_path()));
        assert_eq!(detect_method(&p), INSTALL_METHOD_TAR);
        // 其他位置的執行檔仍是 unknown
        let mut q = probe(&root);
        q.exe = Some(root.join("usr/local/bin/pulse"));
        assert_eq!(
            detect_method(&q.with_cwd_from_exe()),
            INSTALL_METHOD_UNKNOWN
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn upgrade_readiness_reports_the_first_missing_condition() {
        let root = ready_root("ready");
        let anchor = root.join("etc/kairos-pulse/allowed_signers");
        let p = probe(&root);
        let d = detect(true, &anchor, &p);
        assert_eq!(d.upgrade, INSTALL_UPGRADE_READY, "{d:?}");
        assert_eq!(d.method, INSTALL_METHOD_TAR);
        assert_eq!(d.fingerprints.len(), 1);
        assert_eq!(detect(false, &anchor, &p).upgrade, INSTALL_UPGRADE_DISABLED);

        // 沒有啟用 path unit
        std::fs::remove_file(root.join("etc/systemd/system/paths.target.wants/pulse-upgrade.path"))
            .unwrap();
        assert_eq!(detect(true, &anchor, &p).upgrade, INSTALL_UPGRADE_NO_HELPER);

        // 信任錨權限過寬、不存在、沒有可用的金鑰
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o666)).unwrap();
        let d = detect(true, &anchor, &p);
        assert_eq!(d.upgrade, INSTALL_UPGRADE_NO_TRUST_ANCHOR);
        assert!(d.reason.unwrap().contains("權限過寬"));
        std::fs::write(&anchor, "release@kairos-pulse ssh-rsa AAAA\n").unwrap();
        std::fs::set_permissions(&anchor, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            detect(true, &anchor, &p).upgrade,
            INSTALL_UPGRADE_NO_TRUST_ANCHOR
        );
        assert_eq!(
            detect(true, &root.join("missing"), &p).upgrade,
            INSTALL_UPGRADE_NO_TRUST_ANCHOR
        );
        // 正式環境要求 root 擁有（測試以一般使用者執行時一定不符）
        let mut strict = probe(&root);
        strict.require_root_owner = true;
        std::fs::write(&anchor, SIGNERS).unwrap();
        // SAFETY: 只讀取目前行程的有效 uid
        if unsafe { libc::geteuid() } != 0 {
            assert!(
                load_trust_anchor(&anchor, true)
                    .unwrap_err()
                    .contains("root")
            );
        }

        // 不支援：多實例、容器、沒有 systemd、未知安裝方式
        let mut multi = probe(&root);
        multi.state_directory = Some(PathBuf::from("/var/lib/pulse/2"));
        assert_eq!(
            detect(true, &anchor, &multi).upgrade,
            INSTALL_UPGRADE_UNSUPPORTED
        );
        write(&root, ".dockerenv", "");
        assert_eq!(
            detect(true, &anchor, &p).upgrade,
            INSTALL_UPGRADE_UNSUPPORTED
        );
        std::fs::remove_file(root.join(".dockerenv")).unwrap();
        std::fs::remove_dir_all(root.join("run/systemd")).unwrap();
        assert_eq!(
            detect(true, &anchor, &p).upgrade,
            INSTALL_UPGRADE_UNSUPPORTED
        );
        let mut unknown = probe(&root);
        unknown.cwd = None;
        assert_eq!(
            detect(true, &anchor, &unknown).upgrade,
            INSTALL_UPGRADE_UNSUPPORTED
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `install.upgrade = "unsafe_path"`（0.10.0）：其他條件都就緒的主機，事先做輔助程式第一步的路徑檢查
    #[cfg(target_os = "linux")]
    #[test]
    fn a_ready_host_with_unsafe_paths_reports_unsafe_path() {
        let unsafe_reason = |root: &Path| {
            let anchor = root.join("etc/kairos-pulse/allowed_signers");
            let d = detect(true, &anchor, &probe(root));
            assert_eq!(d.upgrade, INSTALL_UPGRADE_UNSAFE_PATH, "{d:?}");
            // 其他欄位不受影響：安裝方式、信任錨的指紋照常回報
            assert_eq!(d.method, INSTALL_METHOD_TAR);
            assert_eq!(d.fingerprints.len(), 1);
            let reason = d.reason.unwrap();
            assert!(reason.starts_with(UNSAFE_PATH_REASON_PREFIX), "{reason}");
            reason
        };
        let ready = |root: &Path, label: &str| {
            let anchor = root.join("etc/kairos-pulse/allowed_signers");
            let d = detect(true, &anchor, &probe(root));
            assert_eq!(d.upgrade, INSTALL_UPGRADE_READY, "{label}: {d:?}");
            assert_eq!(d.reason, None, "{label}");
        };

        // 合規：工作目錄的根目錄還不存在（輔助程式第一次執行時才建立）、0700、或只多了讀取／執行權限
        let root = ready_root("unsafe-ok");
        ready(&root, "沒有 /var/lib/pulse-upgrade");
        assert!(
            !root.join("var/lib/pulse-upgrade").exists(),
            "預檢不建立目錄"
        );
        std::fs::create_dir(root.join("var/lib/pulse-upgrade")).unwrap();
        chmod(&root.join("var/lib/pulse-upgrade"), 0o700);
        ready(&root, "0700");
        chmod(&root.join("var/lib/pulse-upgrade"), 0o755);
        ready(&root, "0755（輔助程式會改回 0700）");
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("var/lib/pulse-upgrade"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "預檢不修改權限");
        }
        // INSTALL 的 root:pulse 751、上層目錄是 sticky 的 1777、pulse.prev 是一般檔案
        chmod(&root.join("opt/pulse"), 0o751);
        chmod(&root.join("opt"), 0o1777);
        write(&root, "opt/pulse/pulse.prev", "VERSION=old\n");
        ready(&root, "751、sticky 的上層目錄、pulse.prev");
        let _ = std::fs::remove_dir_all(&root);

        // 不合規：每一種各自獨立
        type Break = fn(&Path);
        let cases: [(&str, &str, Break); 9] = [
            ("install-group-writable", "opt/pulse: writable", |r| {
                chmod(&r.join("opt/pulse"), 0o775)
            }),
            ("install-other-writable", "opt/pulse: writable", |r| {
                chmod(&r.join("opt/pulse"), 0o757)
            }),
            ("install-symlink", "opt/pulse: is a symbolic link", |r| {
                std::fs::rename(r.join("opt/pulse"), r.join("opt/real")).unwrap();
                std::os::unix::fs::symlink(r.join("opt/real"), r.join("opt/pulse")).unwrap();
            }),
            ("parent-writable", "opt: writable", |r| {
                chmod(&r.join("opt"), 0o777)
            }),
            ("binary-writable", "opt/pulse/pulse: writable", |r| {
                chmod(&r.join("opt/pulse/pulse"), 0o775)
            }),
            (
                "binary-symlink",
                "opt/pulse/pulse: not a regular file",
                |r| {
                    std::fs::rename(r.join("opt/pulse/pulse"), r.join("opt/pulse/pulse.real"))
                        .unwrap();
                    std::os::unix::fs::symlink("pulse.real", r.join("opt/pulse/pulse")).unwrap();
                },
            ),
            ("prev-symlink", "pulse.prev: not a regular file", |r| {
                std::os::unix::fs::symlink("pulse", r.join("opt/pulse/pulse.prev")).unwrap();
            }),
            ("work-root-writable", "pulse-upgrade: writable", |r| {
                std::fs::create_dir(r.join("var/lib/pulse-upgrade")).unwrap();
                chmod(&r.join("var/lib/pulse-upgrade"), 0o770);
            }),
            (
                "work-root-symlink",
                "pulse-upgrade: is a symbolic link",
                |r| {
                    std::fs::create_dir(r.join("var/elsewhere")).unwrap();
                    chmod(&r.join("var/elsewhere"), 0o700);
                    std::os::unix::fs::symlink(
                        r.join("var/elsewhere"),
                        r.join("var/lib/pulse-upgrade"),
                    )
                    .unwrap();
                },
            ),
        ];
        for (name, expect, break_it) in cases {
            let root = ready_root(&format!("unsafe-{name}"));
            ready(&root, name);
            break_it(&root);
            let reason = unsafe_reason(&root);
            assert!(reason.contains(expect), "{name}: {reason}");
            let _ = std::fs::remove_dir_all(&root);
        }

        // 順序：unsafe_path 只在「原本會是 ready」時回報——停用、缺少輔助程式等原因優先
        let root = ready_root("unsafe-order");
        let anchor = root.join("etc/kairos-pulse/allowed_signers");
        chmod(&root.join("opt/pulse"), 0o775);
        let p = probe(&root);
        assert_eq!(
            detect(true, &anchor, &p).upgrade,
            INSTALL_UPGRADE_UNSAFE_PATH
        );
        assert_eq!(detect(false, &anchor, &p).upgrade, INSTALL_UPGRADE_DISABLED);
        std::fs::remove_file(root.join("etc/systemd/system/paths.target.wants/pulse-upgrade.path"))
            .unwrap();
        assert_eq!(detect(true, &anchor, &p).upgrade, INSTALL_UPGRADE_NO_HELPER);
        assert_eq!(
            detect(true, &root.join("missing"), &p).upgrade,
            INSTALL_UPGRADE_NO_TRUST_ANCHOR
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 服務帳號（不是 root）看不到的路徑不算不安全：不因為權限不足而誤報
    #[cfg(target_os = "linux")]
    #[test]
    fn paths_the_service_account_cannot_see_are_not_reported_as_unsafe() {
        // SAFETY: 只讀取目前行程的有效 uid
        if unsafe { libc::geteuid() } == 0 {
            // root 不受目錄權限限制，無法模擬「看不到」
            return;
        }
        let root = ready_root("unsafe-denied");
        let anchor = root.join("etc/kairos-pulse/allowed_signers");
        let p = probe(&root);
        // /var/lib 進不去：lstat /var/lib/pulse-upgrade 得到 EACCES
        std::fs::create_dir(root.join("var/lib/pulse-upgrade")).unwrap();
        chmod(&root.join("var/lib/pulse-upgrade"), 0o777);
        assert_eq!(
            detect(true, &anchor, &p).upgrade,
            INSTALL_UPGRADE_UNSAFE_PATH,
            "看得到時回報"
        );
        chmod(&root.join("var/lib"), 0o000);
        let d = detect(true, &anchor, &p);
        chmod(&root.join("var/lib"), 0o755);
        assert_eq!(d.upgrade, INSTALL_UPGRADE_READY, "{d:?}");
        // 正式環境要求 root 擁有：測試帳號擁有的路徑不通過（與輔助程式相同）
        let why = crate::remote::upgrade::helper::precheck_paths(
            &root.join("opt/pulse"),
            &root.join("var/lib/pulse-upgrade-missing"),
            true,
        )
        .unwrap_err();
        assert!(why.contains("not root"), "{why}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trust_anchor_rejects_symlinks_and_oversized_files() {
        let root = tmp("anchor");
        let big = root.join("big");
        std::fs::write(&big, "#".repeat(MAX_ALLOWED_SIGNERS_BYTES as usize + 1)).unwrap();
        assert!(load_trust_anchor(&big, false).unwrap_err().contains("超過"));
        #[cfg(unix)]
        {
            let real = root.join("real");
            std::fs::write(&real, SIGNERS).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load_trust_anchor(&real, false).is_ok());
            let link = root.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(
                load_trust_anchor(&link, false)
                    .unwrap_err()
                    .contains("符號連結")
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
