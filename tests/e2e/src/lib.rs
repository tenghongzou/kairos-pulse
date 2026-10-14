//! Kairos + Pulse 端對端測試的輔助程式（審計 Q1）。
//!
//! 測試本身在 `tests/` 下；這裡只放共用的部分：
//!
//! - [`bin`]：找到（必要時先建置）實際的 `kairos`、`pulse` 執行檔。
//! - [`Proc`]：啟動子行程、收集 stdout／stderr、等待日誌、送出結束訊號（Unix 為 SIGTERM，
//!   Windows 為 Ctrl+Break）、等待結束代碼，並在測試失敗或結束時清掉行程。
//! - [`TestDir`]：每個測試獨立的暫存目錄（測試失敗時保留，方便查看日誌）。
//! - [`Kairos`]、[`Pulse`]：在暫存目錄中寫好設定、憑證與 `agent.id`，以隨機埠在 127.0.0.1 上啟動。
//! - [`test_pki`]：與 Kairos 整合測試共用的 mTLS 測試 PKI（客戶端憑證、私鑰、CA 與 CRL）。
//! - [`RawClient`]：以 mTLS 連上 Kairos、逐行收送 JSON 的假 client（模擬只支援協定 v1 的舊版 Pulse）。
//! - [`Api`]、[`ApiStream`]：主控台 API（`kairos/API.md`）的 HTTPS 用戶端與 SSE 串流。
//!
//! 不連外部網路、不新增外部套件（只用標準函式庫與 workspace 已使用的 `serde_json`、`rustls`）。

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// mTLS 測試 PKI：直接沿用 Kairos 整合測試的那一份（同一個檔案，不另外複製私鑰）。
///
/// `testdata/pki` 只有憑證、沒有客戶端私鑰，無法實際完成 mTLS 握手，因此客戶端材料取自這裡。
/// 伺服器憑證則用 `pulse/testdata/server.pem`（`CA:FALSE`、由 `server-ca.pem` 簽發）：
/// `kairos/testdata/pki/server.crt` 是 `CA:TRUE` 的自簽憑證，Pulse 的 webpki 不接受它作為伺服器憑證。
#[path = "../../../kairos/src/test_pki.rs"]
pub mod test_pki;

/// 等待日誌、檔案內容等條件的預設上限。正常情況下都在數秒內達成；CI 的 Windows runner 較慢，留足餘裕
pub const WAIT: Duration = Duration::from_secs(30);

/// 等待行程結束的上限（兩端的優雅關機都遠低於此：Kairos 看門狗 4 秒、Pulse 1.5 秒）
pub const EXIT_WAIT: Duration = Duration::from_secs(15);

/// workspace 根目錄
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("無法取得 workspace 根目錄")
}

/// `pulse/testdata` 下的測試檔案
pub fn pulse_testdata(name: &str) -> PathBuf {
    workspace_root().join("pulse/testdata").join(name)
}

/// 實際的執行檔路徑（`kairos` 或 `pulse`）。
///
/// 預設在第一次呼叫時以 `cargo build -p kairos -p pulse` 建置到與測試相同的 target 目錄與 profile
/// （已是最新時幾乎不花時間），確保測的是目前的原始碼，而不是殘留的舊執行檔。
/// 設定環境變數 `KP_E2E_BIN_DIR` 時直接使用該目錄中的執行檔，不建置。
pub fn bin(name: &str) -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| match std::env::var_os("KP_E2E_BIN_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => build_binaries(),
    });
    let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(path.is_file(), "找不到執行檔 {}", path.display());
    path
}

/// 以 cargo 建置兩個執行檔，回傳其所在目錄（`<target>/<profile>`）。
fn build_binaries() -> PathBuf {
    // 測試執行檔位於 <target>/<profile>/deps/
    let exe = std::env::current_exe().expect("無法取得測試執行檔路徑");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("測試執行檔不在 <target>/<profile>/deps 之下")
        .to_path_buf();
    let target_dir = profile_dir.parent().expect("找不到 target 目錄");
    let profile = match profile_dir.file_name().and_then(|n| n.to_str()) {
        Some("debug") => "dev".to_string(),
        Some(other) => other.to_string(),
        None => panic!("無法判斷 profile：{}", profile_dir.display()),
    };
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| env!("CARGO").into());
    let started = Instant::now();
    let mut cmd = Command::new(cargo);
    cmd.args([
        "build",
        "--bins",
        "-p",
        "kairos",
        "-p",
        "pulse",
        "--profile",
    ])
    .arg(&profile)
    .env("CARGO_TARGET_DIR", target_dir)
    .current_dir(workspace_root())
    .stdin(Stdio::null());
    // cargo 執行測試時設定的「本套件」變數（CARGO_MANIFEST_DIR 等）不能傳下去：依賴的建置腳本
    // （例如 ring 的 rerun-if-env-changed）會把它們當成環境變更，每次都重新編譯，
    // 也讓之後一般的 cargo build 再重新編譯一次
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(is_per_package_cargo_var) {
            cmd.env_remove(&key);
        }
    }
    let output = cmd.output().expect("無法執行 cargo build");
    assert!(
        output.status.success(),
        "cargo build -p kairos -p pulse 失敗（{}）：\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!(
        "[e2e] 已建置 kairos、pulse（profile {profile}，{:.1} 秒）：{}",
        started.elapsed().as_secs_f64(),
        profile_dir.display()
    );
    profile_dir
}

/// cargo 為「目前執行的套件」設定的環境變數（見 cargo 文件 Environment variables Cargo sets for crates）
fn is_per_package_cargo_var(key: &str) -> bool {
    key.starts_with("CARGO_PKG_")
        || matches!(
            key,
            "CARGO_MANIFEST_DIR"
                | "CARGO_MANIFEST_PATH"
                | "CARGO_CRATE_NAME"
                | "CARGO_BIN_NAME"
                | "CARGO_PRIMARY_PACKAGE"
                | "CARGO_TARGET_TMPDIR"
                | "CARGO_RUSTC_CURRENT_DIR"
                | "OUT_DIR"
        )
}

/// 127.0.0.1 上目前沒有被使用的 TCP 連接埠（綁定 0 由作業系統分配後立即釋放）
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("無法取得空閒的連接埠")
        .port()
}

/// 每隔 50 ms 檢查一次 `cond`，直到成立或超過 `timeout`（回傳是否成立）
pub fn poll_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 原子地寫入檔案：先寫到同目錄的暫存檔再改名（Kairos 熱重載不會讀到寫一半的內容）
pub fn write_atomically(path: &Path, content: &[u8]) {
    let tmp = path.with_extension("tmp-e2e");
    std::fs::write(&tmp, content).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

// ---------------------------------------------------------------------------
// 暫存目錄
// ---------------------------------------------------------------------------

/// 每個測試獨立的暫存目錄。正常結束時刪除；測試失敗（panic）時保留並印出路徑，方便查看日誌。
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "kp-e2e-{name}-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("無法建立暫存目錄");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 建立（若不存在）並回傳子目錄
    pub fn sub(&self, name: &str) -> PathBuf {
        let p = self.path.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("[e2e] 測試失敗，保留暫存目錄：{}", self.path.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// 子行程
// ---------------------------------------------------------------------------

/// 子行程繼承的環境變數中，會影響 Kairos／Pulse 行為的部分（測試一律清掉，只用明確指定的值）
fn is_ambient_setting(key: &str) -> bool {
    key.starts_with("KAIROS__")
        || key.starts_with("PULSE__")
        // 測試掛鉤（debug 建置）：只在測試明確指定時使用
        || key.starts_with("KAIROS_TEST_")
        || key.starts_with("PULSE_TEST_")
        || matches!(
            key,
            "NOTIFY_SOCKET"
                | "JOURNAL_STREAM"
                | "BUILD_COMMIT"
                // pulse status 的符號與色彩（cli-style §3、§4）
                | "PULSE_SYMBOLS"
                | "NO_COLOR"
                | "CLICOLOR_FORCE"
                // systemd 的 StateDirectory=（在服務中執行測試時）：Pulse 的狀態檔預設位置會改用它
                | "STATE_DIRECTORY"
        )
}

/// 一個啟動中的 `kairos` 或 `pulse` 行程。結束時（含測試 panic）若仍在執行則強制終止。
pub struct Proc {
    name: String,
    child: Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
    status: Option<ExitStatus>,
}

fn collect(
    mut src: impl std::io::Read + Send + 'static,
    buf: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match src.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    })
}

impl Proc {
    /// 在 `cwd` 中以 `args` 與 `envs` 啟動 `bin(name)`。stdin 為空、stdout／stderr 由測試收集。
    pub fn spawn(name: &str, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(bin(name));
        cmd.current_dir(cwd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(is_ambient_setting) {
                cmd.env_remove(&key);
            }
        }
        cmd.envs(envs.iter().copied());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // 自成一個行程群組，Ctrl+Break 只送給它（見 Proc::interrupt）
            win::ensure_console();
            cmd.creation_flags(win::CREATE_NEW_PROCESS_GROUP);
        }
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("無法啟動 {name}：{e}"));
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let readers = vec![
            collect(child.stdout.take().unwrap(), stdout.clone()),
            collect(child.stderr.take().unwrap(), stderr.clone()),
        ];
        Self {
            name: name.to_string(),
            child,
            stdout,
            stderr,
            readers,
            status: None,
        }
    }

    /// 到目前為止的 stdout（Kairos 與 Pulse 的日誌都會寫到 stdout）
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout.lock().unwrap()).into_owned()
    }

    /// 到目前為止的 stderr
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    /// stdout 與 stderr 合併（檢查錯誤訊息時使用：日誌初始化前的錯誤只寫到 stderr）
    pub fn output(&self) -> String {
        format!("{}{}", self.stdout(), self.stderr())
    }

    /// stdout 中含有 `pattern` 的行數
    pub fn count(&self, pattern: &str) -> usize {
        self.stdout()
            .lines()
            .filter(|l| l.contains(pattern))
            .count()
    }

    /// stdout 中含有 `pattern` 的行
    pub fn lines_with(&self, pattern: &str) -> Vec<String> {
        self.stdout()
            .lines()
            .filter(|l| l.contains(pattern))
            .map(str::to_string)
            .collect()
    }

    /// 測試失敗時附上的診斷資訊（stdout 與 stderr 的最後一段）
    pub fn dump(&self) -> String {
        fn tail(s: &str) -> &str {
            const MAX: usize = 12_000;
            if s.len() <= MAX {
                return s;
            }
            let mut start = s.len() - MAX;
            while !s.is_char_boundary(start) {
                start += 1;
            }
            &s[start..]
        }
        format!(
            "==== {} stdout ====\n{}\n==== {} stderr ====\n{}",
            self.name,
            tail(&self.stdout()),
            self.name,
            tail(&self.stderr())
        )
    }

    /// 行程 ID
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// 行程是否仍在執行（已結束時記下結束狀態）
    pub fn is_running(&mut self) -> bool {
        if self.status.is_none() {
            self.status = self.child.try_wait().expect("try_wait 失敗");
        }
        self.status.is_none()
    }

    /// 等到 stdout 出現至少 `n` 行含有 `pattern`；逾時或行程提早結束時 panic 並附上日誌
    pub fn wait_for_count(&mut self, pattern: &str, n: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count(pattern) >= n {
                return;
            }
            if !self.is_running() {
                // 行程已結束：讀完剩下的輸出再判斷一次
                self.join_readers();
                if self.count(pattern) >= n {
                    return;
                }
                panic!(
                    "{} 在出現第 {n} 行「{pattern}」之前就結束了（{:?}）\n{}",
                    self.name,
                    self.status,
                    self.dump()
                );
            }
            if Instant::now() >= deadline {
                panic!(
                    "等待 {} 的第 {n} 行「{pattern}」逾時（{:?}）\n{}",
                    self.name,
                    timeout,
                    self.dump()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 等到 stdout 出現含有 `pattern` 的一行
    pub fn wait_for(&mut self, pattern: &str, timeout: Duration) {
        self.wait_for_count(pattern, 1, timeout);
    }

    /// 要求行程優雅結束：Unix 送 SIGTERM，Windows 送 Ctrl+Break（兩個程式都會攔截並走正常關機流程）
    pub fn interrupt(&mut self) {
        if !self.is_running() {
            return;
        }
        #[cfg(unix)]
        {
            let pid = i32::try_from(self.child.id()).expect("pid 超出範圍");
            // SAFETY: kill(2) 只送出訊號，不涉及記憶體；pid 是仍未回收（未 wait）的子行程
            let rc = unsafe { unix::kill(pid, unix::SIGTERM) };
            assert_eq!(rc, 0, "無法送出 SIGTERM 給 {}", self.name);
        }
        #[cfg(windows)]
        win::ctrl_break(self.child.id(), &self.name);
    }

    /// 等待行程結束並回傳結束代碼；逾時則強制終止並 panic
    pub fn wait_exit(&mut self, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        while self.is_running() {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.join_readers();
                panic!(
                    "{} 未在 {:?} 內結束，已強制終止\n{}",
                    self.name,
                    timeout,
                    self.dump()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.join_readers();
        let status = self.status.expect("已結束");
        status
            .code()
            .unwrap_or_else(|| panic!("{} 被訊號終止：{status:?}\n{}", self.name, self.dump()))
    }

    /// 優雅結束並回傳結束代碼與從送出訊號到結束的時間
    pub fn stop(&mut self) -> (i32, Duration) {
        let started = Instant::now();
        self.interrupt();
        let code = self.wait_exit(EXIT_WAIT);
        (code, started.elapsed())
    }

    fn join_readers(&mut self) {
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if self.is_running() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if std::thread::panicking() {
            self.join_readers();
            eprintln!("{}", self.dump());
        }
    }
}

/// 執行到結束（用於啟動即失敗的情境），回傳結束代碼與行程（可再查看輸出）
pub fn run_to_exit(name: &str, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> (i32, Proc) {
    let mut p = Proc::spawn(name, cwd, args, envs);
    let code = p.wait_exit(EXIT_WAIT);
    (code, p)
}

#[cfg(unix)]
mod unix {
    pub const SIGTERM: i32 = 15;
    unsafe extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::sync::Once;

    pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CTRL_BREAK_EVENT: u32 = 1;
    const STD_HANDLES: [u32; 3] = [
        -10i32 as u32, // STD_INPUT_HANDLE
        -11i32 as u32, // STD_OUTPUT_HANDLE
        -12i32 as u32, // STD_ERROR_HANDLE
    ];
    const SW_HIDE: i32 = 0;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
        fn GetConsoleWindow() -> *mut c_void;
        fn AllocConsole() -> i32;
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn SetStdHandle(std_handle: u32, handle: *mut c_void) -> i32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn ShowWindow(hwnd: *mut c_void, cmd_show: i32) -> i32;
    }

    /// Ctrl+Break 只能送給與自己共用主控台的行程。測試行程沒有主控台時（例如由沒有主控台的
    /// 程式啟動），先配置一個隱藏的主控台讓子行程繼承；原本的標準輸入輸出（測試框架的管線）不變。
    pub fn ensure_console() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: 這些 Win32 API 只讀寫行程層級的主控台狀態與標準 handle，參數都是常數或
            // 先前由 GetStdHandle 取得的 handle
            unsafe {
                if !GetConsoleWindow().is_null() {
                    return;
                }
                let saved = STD_HANDLES.map(|h| GetStdHandle(h));
                if AllocConsole() != 0 {
                    for (h, handle) in STD_HANDLES.iter().zip(saved) {
                        SetStdHandle(*h, handle);
                    }
                    let hwnd = GetConsoleWindow();
                    if !hwnd.is_null() {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                }
            }
        });
    }

    /// 對以 `CREATE_NEW_PROCESS_GROUP` 啟動的子行程（行程群組 ID 即其 PID）送出 Ctrl+Break
    pub fn ctrl_break(pid: u32, name: &str) {
        // SAFETY: 只送出主控台事件；pid 是以 CREATE_NEW_PROCESS_GROUP 啟動、仍在執行的子行程
        let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        assert_ne!(
            ok,
            0,
            "無法送出 Ctrl+Break 給 {name}：{}",
            std::io::Error::last_os_error()
        );
    }
}

// ---------------------------------------------------------------------------
// sd_notify 狀態（Linux：Pulse 以 NOTIFY_SOCKET 送出 STATUS=...）
// ---------------------------------------------------------------------------

/// 模擬 systemd 的 `NOTIFY_SOCKET`，收集 Pulse 送出的 `STATUS=` 文字（只在 Unix）。
#[cfg(unix)]
pub struct NotifySocket {
    path: PathBuf,
    messages: Arc<Mutex<Vec<String>>>,
}

#[cfg(unix)]
impl NotifySocket {
    pub fn bind(dir: &Path) -> Self {
        use std::os::unix::net::UnixDatagram;
        let path = dir.join("notify.sock");
        let socket = UnixDatagram::bind(&path).expect("無法建立 NOTIFY_SOCKET");
        let messages = Arc::new(Mutex::new(Vec::new()));
        let sink = messages.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            while let Ok(n) = socket.recv(&mut buf) {
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            }
        });
        Self { path, messages }
    }

    /// 傳給子行程的 `NOTIFY_SOCKET` 值
    pub fn env_value(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// 收到的 `STATUS=` 文字（依收到的順序）
    pub fn statuses(&self) -> Vec<String> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| m.lines().find_map(|l| l.strip_prefix("STATUS=")))
            .map(str::to_string)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Kairos 與 Pulse 的測試環境
// ---------------------------------------------------------------------------

/// Kairos 啟動完成的日誌（之後才開始接受連線）
pub const KAIROS_STARTED: &str = "Kairos 已啟動，監聽";
/// Kairos 優雅關機完成的日誌
pub const KAIROS_STOPPED: &str = "Kairos 已結束";
/// Pulse 註冊成功（收到 hello_ack）的日誌
pub const PULSE_REGISTERED: &str = "已向伺服器註冊";
/// Pulse 優雅結束的日誌
pub const PULSE_STOPPED: &str = "Pulse 已結束";

/// 在暫存目錄中準備好的 Kairos（mTLS + CRL、熱重載每秒檢查一次；主控台 API 在另一個隨機埠）
pub struct Kairos {
    pub dir: PathBuf,
    pub port: u16,
    /// 主控台 API 的連接埠（`console.listen`）
    pub console_port: u16,
}

impl Kairos {
    /// 在 `root/kairos` 準備設定：伺服器憑證、客戶端 CA（[`test_pki::CA`]）、CRL（`crl` 的內容）
    pub fn new(root: &TestDir, crl: &str) -> Self {
        Self::named(root, "kairos", crl)
    }

    /// 同 [`Kairos::new`]，但放在 `root/<name>`（同一個測試準備多份設定時使用）
    pub fn named(root: &TestDir, name: &str, crl: &str) -> Self {
        let dir = root.sub(name);
        let port = free_port();
        // 兩次 free_port 理論上可能拿到同一個埠（之前的監聽已關閉），主控台埠另外確認不同
        let console_port = std::iter::repeat_with(free_port)
            .find(|p| *p != port)
            .unwrap();
        std::fs::copy(pulse_testdata("server.pem"), dir.join("server.pem")).unwrap();
        std::fs::copy(pulse_testdata("server.key"), dir.join("server.key")).unwrap();
        std::fs::write(dir.join("client-ca.pem"), test_pki::CA).unwrap();
        std::fs::write(dir.join("agents.crl"), crl).unwrap();
        std::fs::write(
            dir.join("kairos.toml"),
            format!(
                r#"[server]
listen = "127.0.0.1:{port}"
cert_path = "server.pem"
key_path = "server.key"
max_connections = 64
reload_interval_secs = 1

[storage]
data_dir = "data"
# 預設的 SQLite 之外另輸出 JSONL 鏡像：相容測試檢查兩種 schema 寫在同一個 JSONL 檔
jsonl_mirror = true

[auth]
client_ca = "client-ca.pem"
crl = ["agents.crl"]

[log]
level = "debug"

# 主控台 API：與 agent 連線埠共用伺服器憑證（SAN localhost）
[console]
listen = "127.0.0.1:{console_port}"
"#
            ),
        )
        .unwrap();
        Self {
            dir,
            port,
            console_port,
        }
    }

    /// 以實際的 `kairos api-token create` 建立唯讀 token（Kairos 執行中也可以）
    pub fn create_token(&self, name: &str) -> String {
        let (code, p) = run_to_exit(
            "kairos",
            &self.dir,
            &[
                "api-token",
                "create",
                "--name",
                name,
                "--config",
                "kairos.toml",
                "--json",
            ],
            &[],
        );
        assert_eq!(code, 0, "kairos api-token create 失敗\n{}", p.dump());
        let doc: serde_json::Value = serde_json::from_str(p.stdout().trim())
            .unwrap_or_else(|e| panic!("api-token create --json 不是 JSON（{e}）：{}", p.stdout()));
        assert_eq!(doc["schema"], "kairos.api-token.create/v1", "{doc}");
        assert!(p.stdout().is_ascii());
        doc["data"]["token"]
            .as_str()
            .expect("應有 token")
            .to_string()
    }

    /// 主控台 API 的用戶端
    pub fn api(&self, token: &str) -> Api {
        Api {
            port: self.console_port,
            token: token.to_string(),
        }
    }

    /// 啟動並等到開始監聽
    pub fn start(&self) -> Proc {
        let mut p = Proc::spawn("kairos", &self.dir, &["--config", "kairos.toml"], &[]);
        p.wait_for(KAIROS_STARTED, WAIT);
        p
    }

    /// 以原子改名換掉 CRL（Kairos 在下一個重載週期套用，並重新驗證現役連線）
    pub fn replace_crl(&self, crl: &str) {
        write_atomically(&self.dir.join("agents.crl"), crl.as_bytes());
    }

    /// SQLite（`data/kairos.db`）中所有的 report：以實際的 `kairos reports export` 匯出（Kairos 執行中
    /// 也可以），每行的格式與 JSONL 相同，依寫入順序。資料庫尚未建立時為空
    pub fn reports(&self) -> Vec<serde_json::Value> {
        if !self.dir.join("data").join("kairos.db").exists() {
            return Vec::new();
        }
        let (code, p) = run_to_exit(
            "kairos",
            &self.dir,
            &[
                "reports",
                "export",
                "--config",
                "kairos.toml",
                "--since",
                "7d",
            ],
            &[],
        );
        assert_eq!(code, 0, "kairos reports export 失敗\n{}", p.dump());
        p.stdout()
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("匯出的一行不是 JSON（{e}）：{line}"))
            })
            .collect()
    }

    /// `kairos hosts list --json` 的 `data.hosts`（SQLite 主機清冊）
    pub fn hosts(&self) -> Vec<serde_json::Value> {
        let (code, p) = run_to_exit(
            "kairos",
            &self.dir,
            &["hosts", "list", "--config", "kairos.toml", "--json"],
            &[],
        );
        assert_eq!(code, 0, "kairos hosts list 失敗\n{}", p.dump());
        let doc: serde_json::Value = serde_json::from_str(p.stdout().trim()).unwrap_or_else(|e| {
            panic!("kairos hosts list --json 不是 JSON（{e}）：{}", p.stdout())
        });
        assert_eq!(doc["schema"], "kairos.hosts.list/v1", "{doc}");
        assert_eq!(doc["ok"], true, "{doc}");
        assert!(p.stdout().is_ascii(), "{}", p.stdout());
        doc["data"]["hosts"].as_array().cloned().unwrap_or_default()
    }

    /// `data/reports-*.jsonl`（JSONL 鏡像）中所有的 report（依檔名與行序）
    pub fn jsonl_reports(&self) -> Vec<serde_json::Value> {
        let Ok(entries) = std::fs::read_dir(self.dir.join("data")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("reports-") && n.ends_with(".jsonl"))
            })
            .collect();
        files.sort();
        let mut out = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap_or_default();
            // 最後一行可能還沒寫完（沒有換行），只取完整的行
            let complete = match text.rfind('\n') {
                Some(i) => &text[..=i],
                None => "",
            };
            for line in complete.lines() {
                out.push(
                    serde_json::from_str(line).unwrap_or_else(|e| {
                        panic!("{} 的一行不是 JSON（{e}）：{line}", f.display())
                    }),
                );
            }
        }
        out
    }

    /// `data/agents.json` 的內容
    pub fn agents(&self) -> serde_json::Value {
        let path = self.dir.join("data").join("agents.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("無法讀取 {}：{e}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("agents.json 不是 JSON（{e}）：{text}"))
    }
}

/// Pulse 使用的客戶端身分
#[derive(Debug, Clone, Copy)]
pub struct Identity {
    pub agent_id: &'static str,
    pub cert: &'static str,
    pub key: &'static str,
}

/// 有效的客戶端身分（未撤銷）
pub const GOOD: Identity = Identity {
    agent_id: test_pki::GOOD_ID,
    cert: test_pki::GOOD_CERT,
    key: test_pki::GOOD_KEY,
};

/// 會被 [`test_pki::CRL_REVOKE`] 撤銷的客戶端身分
pub const REVOKED: Identity = Identity {
    agent_id: test_pki::REVOKED_ID,
    cert: test_pki::REVOKED_CERT,
    key: test_pki::REVOKED_KEY,
};

/// 在暫存目錄中準備好的 Pulse：心跳與 report 每秒一次、重連延遲 1～2 秒（測試用的短設定）
pub struct Pulse {
    pub dir: PathBuf,
}

impl Pulse {
    /// 在 `root/<name>` 準備連到 `port` 的設定、客戶端憑證與 `agent.id`
    pub fn new(root: &TestDir, name: &str, port: u16, identity: Identity) -> Self {
        let dir = root.sub(name);
        std::fs::copy(pulse_testdata("server-ca.pem"), dir.join("server-ca.pem")).unwrap();
        std::fs::write(dir.join("client.pem"), identity.cert).unwrap();
        std::fs::write(dir.join("client.key"), identity.key).unwrap();
        std::fs::write(dir.join("agent.id"), format!("{}\n", identity.agent_id)).unwrap();
        std::fs::write(
            dir.join("pulse.toml"),
            format!(
                r#"[server]
address = "127.0.0.1"
port = {port}
domain = "localhost"
ca_cert = "server-ca.pem"
client_cert = "client.pem"
client_key = "client.key"

[retry]
initial_delay = 1
max_delay = 2

[heartbeat]
interval = 1

[report]
interval = 1

[status]
file = "state/status.json"

[log]
level = "trace"
"#
            ),
        )
        .unwrap();
        Self { dir }
    }

    /// 啟動（不等待註冊）
    pub fn start(&self, envs: &[(&str, &str)]) -> Proc {
        Proc::spawn("pulse", &self.dir, &[], envs)
    }
}

// ---------------------------------------------------------------------------
// 主控台 API（HTTPS）
// ---------------------------------------------------------------------------

/// 以伺服器端 TLS（信任 `pulse/testdata/server-ca.pem`、名稱 localhost）連上主控台
fn api_connect(port: u16) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName};
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(pulse_testdata("server-ca.pem")).unwrap() {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let conn =
        rustls::ClientConnection::new(Arc::new(config), ServerName::try_from("localhost").unwrap())
            .unwrap();
    let tcp = std::net::TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("無法連上主控台 127.0.0.1:{port}：{e}"));
    tcp.set_read_timeout(Some(WAIT)).unwrap();
    rustls::StreamOwned::new(conn, tcp)
}

/// 送出一個完整的 HTTP/1.0 請求（含標頭與本文）到主控台；回傳狀態碼、標頭與本文
pub fn api_raw(port: u16, request: &str) -> (u16, String, String) {
    use std::io::{Read as _, Write as _};
    let mut s = api_connect(port);
    s.write_all(request.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("回應沒有標頭：{text}"));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

/// 主控台 API 的用戶端（每個請求一條 HTTP/1.0 連線）
pub struct Api {
    pub port: u16,
    pub token: String,
}

impl Api {
    /// `GET path`，回傳狀態碼與 JSON 本文
    pub fn get(&self, path: &str) -> (u16, serde_json::Value) {
        use std::io::{Read as _, Write as _};
        let mut s = api_connect(self.port);
        write!(
            s,
            "GET {path} HTTP/1.0\r\nHost: localhost\r\nAuthorization: Bearer {}\r\n\r\n",
            self.token
        )
        .unwrap();
        s.flush().unwrap();
        let mut buf = Vec::new();
        // 伺服器關閉時可能沒有 close_notify：已讀到的內容照常處理
        let _ = s.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("回應沒有標頭：{text}"));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let json =
            serde_json::from_str(body).unwrap_or_else(|e| panic!("回應不是 JSON（{e}）：{text}"));
        (status, json)
    }

    /// `GET path` 並要求 200，回傳 `data`
    pub fn data(&self, path: &str) -> serde_json::Value {
        let (status, v) = self.get(path);
        assert_eq!(status, 200, "GET {path}：{v}");
        assert!(
            v["server_time"].as_str().is_some_and(|t| t.ends_with('Z')),
            "{v}"
        );
        v["data"].clone()
    }
}

/// SSE 的一則事件
#[derive(Debug, Clone)]
pub struct ApiEvent {
    pub id: Option<String>,
    pub event: String,
    pub data: serde_json::Value,
}

/// `/api/v1/events` 的串流
pub struct ApiStream {
    stream: rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
    buf: String,
}

impl ApiStream {
    pub fn open(api: &Api, last_event_id: Option<&str>) -> Self {
        use std::io::{Read as _, Write as _};
        let mut s = api_connect(api.port);
        let resume = last_event_id
            .map(|id| format!("Last-Event-ID: {id}\r\n"))
            .unwrap_or_default();
        write!(
            s,
            "GET /api/v1/events HTTP/1.0\r\nHost: localhost\r\nAccept: text/event-stream\r\nAuthorization: Bearer {}\r\n{resume}\r\n",
            api.token
        )
        .unwrap();
        s.flush().unwrap();
        let mut raw = Vec::new();
        let split = loop {
            if let Some(p) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break p;
            }
            let mut chunk = [0u8; 4096];
            let n = s.read(&mut chunk).expect("讀取 SSE 標頭失敗");
            assert!(n > 0, "主控台在標頭之前關閉了連線");
            raw.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        assert!(
            head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200"),
            "{head}"
        );
        Self {
            stream: s,
            buf: String::from_utf8_lossy(&raw[split + 4..]).into_owned(),
        }
    }

    /// 下一則事件（略過心跳註解與 `retry:`；讀取逾時為 [`WAIT`]）
    pub fn next_event(&mut self) -> ApiEvent {
        use std::io::Read as _;
        loop {
            while let Some(p) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..p + 2).collect();
                let (mut id, mut event, mut data) = (None, None, String::new());
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("id: ") {
                        id = Some(v.to_string());
                    } else if let Some(v) = line.strip_prefix("event: ") {
                        event = Some(v.to_string());
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data.push_str(v);
                    }
                }
                if let Some(event) = event {
                    return ApiEvent {
                        id,
                        event,
                        data: serde_json::from_str(&data).unwrap_or(serde_json::Value::Null),
                    };
                }
            }
            let mut chunk = [0u8; 4096];
            let n = self
                .stream
                .read(&mut chunk)
                .expect("等待 SSE 事件逾時或失敗");
            assert!(n > 0, "SSE 串流已結束");
            self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    }

    /// 等到某種事件且 `agent_id` 相符（略過其他的）
    pub fn wait_for(&mut self, kind: &str, agent_id: &str) -> ApiEvent {
        loop {
            let e = self.next_event();
            if e.event == kind && (agent_id.is_empty() || e.data["agent_id"] == agent_id) {
                return e;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 假 client（協定 v1）
// ---------------------------------------------------------------------------

/// 以 mTLS 連上 Kairos、逐行收送 JSON 的假 client。
///
/// 新版 Pulse 一律宣告 `proto_min` 1～`proto_max` 2，沒有只用 v1 的設定；舊版（0.1.0）Pulse 的
/// 執行檔也不在 repo 中，因此以這個 client 送出與舊版 Pulse 相同的訊息，驗證新版 Kairos 對 v1 的相容。
/// TLS 設定與 Pulse 相同：以 `pulse/testdata/server-ca.pem` 驗證伺服器（`localhost`），出示客戶端憑證。
pub struct RawClient {
    reader: std::io::BufReader<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>,
}

impl RawClient {
    /// 連上 `127.0.0.1:port` 並完成 TLS 握手（讀取逾時為 [`WAIT`]）
    pub fn connect(port: u16, identity: Identity) -> Self {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_file_iter(pulse_testdata("server-ca.pem")).unwrap() {
            roots.add(cert.unwrap()).unwrap();
        }
        let chain: Vec<CertificateDer<'static>> =
            CertificateDer::pem_slice_iter(identity.cert.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap();
        let key = PrivateKeyDer::from_pem_slice(identity.key.as_bytes()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .unwrap();
        let conn = rustls::ClientConnection::new(
            Arc::new(config),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let tcp = std::net::TcpStream::connect(("127.0.0.1", port))
            .unwrap_or_else(|e| panic!("無法連上 127.0.0.1:{port}：{e}"));
        tcp.set_read_timeout(Some(WAIT)).unwrap();
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            stream
                .conn
                .complete_io(&mut stream.sock)
                .unwrap_or_else(|e| panic!("TLS 握手失敗：{e}"));
        }
        Self {
            reader: std::io::BufReader::new(stream),
        }
    }

    /// 送出一行 JSON
    pub fn send(&mut self, msg: &serde_json::Value) {
        use std::io::Write as _;
        let stream = self.reader.get_mut();
        writeln!(stream, "{msg}").unwrap();
        stream.flush().unwrap();
    }

    /// 讀取一行 JSON（逾時或連線關閉時 panic）
    pub fn recv(&mut self) -> serde_json::Value {
        use std::io::BufRead as _;
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("讀取 Kairos 的回應失敗：{e}"));
        assert!(n > 0, "Kairos 關閉了連線");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("回應不是 JSON（{e}）：{line}"))
    }

    /// 送出 close_notify 後關閉（與 Pulse 的優雅斷線相同）
    pub fn close(mut self) {
        use std::io::Write as _;
        let stream = self.reader.get_mut();
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }
}
