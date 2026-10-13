//! 讓 systemd／journal 看得到 Pulse 的服務狀態與需要人工處理的錯誤。
//!
//! Pulse 的日誌只寫到 stdout 與 `logs/`，`pulse.service` 又設定 `StandardOutput=null`，
//! 因此 journal 只看得到 stderr。憑證被撤銷、身分驗證失敗等錯誤會讓 Pulse 無限重連，
//! 在 systemd 看來仍是 `active (running)`；只看 `systemctl status` 或 journal 的監控會漏掉。
//!
//! - [`attention`]：需要人工處理的錯誤在寫日誌的同時，也寫一行到 stderr（節流由呼叫端負責，
//!   與 ERROR 日誌相同）。stdout 與 stderr 都是終端機時（互動執行，日誌已經顯示在畫面上）不重複寫。
//!   stderr 接到 journal（有 `JOURNAL_STREAM` 環境變數）時在行首加上 `<3>`，journal 會以 err
//!   等級記錄（`journalctl -u pulse -p err` 可篩選），行首的 `<3>` 不會出現在訊息中。
//! - [`set_status`]：Unix 上有 `NOTIFY_SOCKET` 環境變數時，以 sd_notify 協定送出 `STATUS=...`，
//!   `systemctl status` 會顯示「Status: "..."」（unit 需設定 `NotifyAccess=main`）。
//!   沒有 `NOTIFY_SOCKET` 或送出失敗時靜默略過；Windows 不做任何事。
//!
//! 測試時兩者都不會實際寫出，而是記在目前執行緒的紀錄中（見 `capture`）。

/// 寫到 stderr 的一行。`journal` 為 true 時加上 syslog 等級前綴 `<3>`（err），
/// systemd 的 `SyslogLevelPrefix=`（預設開啟）會把它轉成 journal 的等級。
fn stderr_line(msg: &str, journal: bool) -> String {
    let prefix = if journal { "<3>" } else { "" };
    format!("{prefix}[ERROR] {msg}")
}

/// 是否要把訊息另外寫到 stderr：stdout 與 stderr 都是終端機時，日誌（stdout）已經顯示在同一個畫面上，
/// 不重複寫；其他情況（systemd 的 journal、重新導向到檔案、Windows 服務等）都寫。
fn should_mirror_to_stderr(stdout_is_terminal: bool, stderr_is_terminal: bool) -> bool {
    !(stdout_is_terminal && stderr_is_terminal)
}

/// 需要人工處理的錯誤：在寫日誌的同時也寫一行到 stderr（見模組說明）。
///
/// `msg` 必須已是安全的單行文字（伺服器送來的內容已經過清理）。呼叫端負責節流。
pub fn attention(msg: &str) {
    imp::attention(msg);
}

/// 更新 systemd 的服務狀態文字（sd_notify `STATUS=`；見模組說明）。
pub fn set_status(text: &str) {
    imp::set_status(text);
}

#[cfg(not(test))]
mod imp {
    use std::io::{IsTerminal as _, Write as _};
    use std::sync::OnceLock;

    use super::{should_mirror_to_stderr, stderr_line};

    pub fn attention(msg: &str) {
        // Some(journal)：要寫 stderr，且 stderr 是否接到 journal；None：不寫
        static MODE: OnceLock<Option<bool>> = OnceLock::new();
        let mode = *MODE.get_or_init(|| {
            should_mirror_to_stderr(
                std::io::stdout().is_terminal(),
                std::io::stderr().is_terminal(),
            )
            .then(stderr_is_journal)
        });
        if let Some(journal) = mode {
            // 寫入失敗（例如 stderr 已關閉）時忽略；不用 eprintln!，它在寫入失敗時會 panic
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "{}", stderr_line(msg, journal));
        }
    }

    /// systemd 把 stdout / stderr 接到 journal 時會設定 `JOURNAL_STREAM`。
    fn stderr_is_journal() -> bool {
        cfg!(unix) && std::env::var_os("JOURNAL_STREAM").is_some_and(|v| !v.is_empty())
    }

    #[cfg(unix)]
    pub fn set_status(text: &str) {
        static NOTIFIER: OnceLock<Option<super::sd_notify::Notifier>> = OnceLock::new();
        if let Some(notifier) = NOTIFIER.get_or_init(super::sd_notify::Notifier::from_env)
            && !notifier.send_status(text)
        {
            log::trace!("sd_notify 送出 STATUS 失敗，略過");
        }
    }

    #[cfg(not(unix))]
    pub fn set_status(_text: &str) {}
}

/// sd_notify 協定（`man sd_notify`）的最小實作：以 `AF_UNIX` datagram 把 `STATUS=...`
/// 送到 `NOTIFY_SOCKET` 指定的位址（檔案系統路徑，或 `@` 開頭的抽象命名空間）。
#[cfg(unix)]
mod sd_notify {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::net::UnixDatagram;
    use std::path::PathBuf;

    /// `NOTIFY_SOCKET` 指定的位址。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum NotifyTarget {
        /// 檔案系統中的 socket（例如 `/run/systemd/notify`）
        Path(PathBuf),
        /// 抽象命名空間（`@` 開頭，不含 `@` 本身；Linux 限定）
        Abstract(Vec<u8>),
    }

    /// 解析 `NOTIFY_SOCKET`：`/` 開頭為路徑、`@` 開頭為抽象命名空間；其他（空字串、
    /// `vsock:` 等本程式不支援的形式）回傳 `None`。
    pub fn parse_notify_socket(value: &OsStr) -> Option<NotifyTarget> {
        let bytes = value.as_bytes();
        match bytes.first()? {
            b'/' => Some(NotifyTarget::Path(PathBuf::from(value))),
            b'@' if bytes.len() > 1 => Some(NotifyTarget::Abstract(bytes[1..].to_vec())),
            _ => None,
        }
    }

    /// `STATUS=` 訊息。換行在 sd_notify 中是欄位分隔字元，換成空白，避免夾帶其他欄位。
    pub fn status_message(text: &str) -> String {
        let text: String = text
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
        format!("STATUS={text}")
    }

    /// 送往 `NOTIFY_SOCKET` 的未綁定 datagram socket。
    #[derive(Debug)]
    pub struct Notifier {
        socket: UnixDatagram,
        target: NotifyTarget,
    }

    impl Notifier {
        /// 依 `NOTIFY_SOCKET` 建立；沒有設定、格式不支援或無法建立 socket 時回傳 `None`。
        #[cfg_attr(test, allow(dead_code))]
        pub fn from_env() -> Option<Self> {
            Self::new(parse_notify_socket(&std::env::var_os("NOTIFY_SOCKET")?)?)
        }

        pub fn new(target: NotifyTarget) -> Option<Self> {
            let socket = UnixDatagram::unbound().ok()?;
            Some(Self { socket, target })
        }

        /// 送出 `STATUS=<text>`；回傳是否成功送出（失敗時由呼叫端決定是否略過）。
        pub fn send_status(&self, text: &str) -> bool {
            self.send(status_message(text).as_bytes())
        }

        fn send(&self, msg: &[u8]) -> bool {
            match &self.target {
                NotifyTarget::Path(path) => self.socket.send_to(msg, path).is_ok(),
                #[cfg(any(target_os = "linux", target_os = "android"))]
                NotifyTarget::Abstract(name) => {
                    #[cfg(target_os = "android")]
                    use std::os::android::net::SocketAddrExt as _;
                    #[cfg(target_os = "linux")]
                    use std::os::linux::net::SocketAddrExt as _;
                    std::os::unix::net::SocketAddr::from_abstract_name(name)
                        .and_then(|addr| self.socket.send_to_addr(msg, &addr))
                        .is_ok()
                }
                // 其他 Unix 沒有抽象命名空間
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                NotifyTarget::Abstract(_) => false,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::Duration;

        #[test]
        fn notify_socket_parsing() {
            assert_eq!(
                parse_notify_socket(OsStr::new("/run/systemd/notify")),
                Some(NotifyTarget::Path(PathBuf::from("/run/systemd/notify")))
            );
            assert_eq!(
                parse_notify_socket(OsStr::new("@/org/freedesktop/systemd1/notify/123")),
                Some(NotifyTarget::Abstract(
                    b"/org/freedesktop/systemd1/notify/123".to_vec()
                ))
            );
            for unsupported in ["", "@", "relative/path", "vsock:2:1234"] {
                assert_eq!(
                    parse_notify_socket(OsStr::new(unsupported)),
                    None,
                    "{unsupported}"
                );
            }
        }

        #[test]
        fn status_message_is_a_single_field() {
            assert_eq!(status_message("已註冊：a:1"), "STATUS=已註冊：a:1");
            assert_eq!(
                status_message("a\nREADY=1\r\nb"),
                "STATUS=a READY=1  b",
                "換行不可夾帶其他欄位"
            );
        }

        fn recv_text(server: &UnixDatagram) -> String {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buf = [0u8; 4096];
            let n = server.recv(&mut buf).unwrap();
            String::from_utf8(buf[..n].to_vec()).unwrap()
        }

        #[test]
        fn sends_status_to_path_socket() {
            let dir = std::env::temp_dir().join(format!(
                "pulse-notify-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("notify");
            let server = UnixDatagram::bind(&path).unwrap();
            let notifier = Notifier::new(
                parse_notify_socket(path.as_os_str()).expect("路徑形式的 NOTIFY_SOCKET"),
            )
            .unwrap();
            assert!(notifier.send_status("已註冊：127.0.0.1:18443"));
            assert_eq!(recv_text(&server), "STATUS=已註冊：127.0.0.1:18443");
            assert!(notifier.send_status("重新連線中"));
            assert_eq!(recv_text(&server), "STATUS=重新連線中");

            // 接收端消失後送出失敗：回傳 false，不 panic
            drop(server);
            std::fs::remove_dir_all(&dir).unwrap();
            assert!(!notifier.send_status("重新連線中"));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn sends_status_to_abstract_socket() {
            use std::os::linux::net::SocketAddrExt as _;
            let name = format!(
                "pulse-notify-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            );
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
            let server = UnixDatagram::bind_addr(&addr).unwrap();
            let target = parse_notify_socket(OsStr::new(&format!("@{name}"))).unwrap();
            assert_eq!(target, NotifyTarget::Abstract(name.clone().into_bytes()));
            let notifier = Notifier::new(target).unwrap();
            assert!(notifier.send_status("無法註冊：certificate_rejected（x）"));
            assert_eq!(
                recv_text(&server),
                "STATUS=無法註冊：certificate_rejected（x）"
            );
        }

        #[test]
        fn missing_socket_is_silently_ignored() {
            let notifier = Notifier::new(NotifyTarget::Path(PathBuf::from(
                "/nonexistent/pulse/notify-socket",
            )))
            .unwrap();
            assert!(!notifier.send_status("重新連線中"));
        }
    }
}

/// 測試用：不實際寫出，記在目前執行緒的紀錄中。
#[cfg(test)]
mod imp {
    use std::cell::RefCell;

    use super::Event;

    thread_local! {
        static RECORDED: RefCell<Option<Vec<Event>>> = const { RefCell::new(None) };
    }

    fn record(event: Event) {
        RECORDED.with(|r| {
            if let Some(events) = r.borrow_mut().as_mut() {
                events.push(event);
            }
        });
    }

    pub fn attention(msg: &str) {
        record(Event::Stderr(msg.to_string()));
    }

    pub fn set_status(text: &str) {
        record(Event::Status(text.to_string()));
    }

    pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<Event>) {
        RECORDED.with(|r| *r.borrow_mut() = Some(Vec::new()));
        let out = f();
        let events = RECORDED.with(|r| r.borrow_mut().take().unwrap_or_default());
        (out, events)
    }
}

/// 測試時記錄下來的輸出。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 寫到 stderr 的訊息（不含前綴）
    Stderr(String),
    /// 送出的 STATUS 文字
    Status(String),
}

/// 測試用：執行 `f`，回傳其結果與期間本執行緒的 stderr／STATUS 輸出。
#[cfg(test)]
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<Event>) {
    imp::capture(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_line_has_journal_priority_prefix_only_for_journal() {
        assert_eq!(stderr_line("x", false), "[ERROR] x");
        assert_eq!(stderr_line("x", true), "<3>[ERROR] x");
    }

    #[test]
    fn stderr_mirror_is_skipped_only_on_an_interactive_terminal() {
        // systemd（stdout=null、stderr=journal）、重新導向、Windows 服務：寫
        assert!(should_mirror_to_stderr(false, false));
        assert!(should_mirror_to_stderr(true, false));
        assert!(should_mirror_to_stderr(false, true));
        // 兩者都是終端機：日誌已顯示在同一個畫面上，不重複
        assert!(!should_mirror_to_stderr(true, true));
    }

    #[test]
    fn unit_file_matches_the_status_implementation() {
        // O2：程式只以 sd_notify 送 STATUS=（不送 READY=1），unit 必須是 Type=simple 並設定
        // NotifyAccess=main（否則 systemd 不設 NOTIFY_SOCKET、也不接受通知），且允許 AF_UNIX
        let unit = include_str!("../packaging/pulse.service");
        let directives: Vec<&str> = unit
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#') && l.contains('='))
            .collect();
        assert!(directives.contains(&"Type=simple"), "{directives:?}");
        assert!(directives.contains(&"NotifyAccess=main"), "{directives:?}");
        assert!(
            !directives
                .iter()
                .any(|d| d.starts_with("Type=") && *d != "Type=simple"),
            "{directives:?}"
        );
        let families = directives
            .iter()
            .find_map(|d| d.strip_prefix("RestrictAddressFamilies="))
            .expect("RestrictAddressFamilies");
        assert!(
            families.split_whitespace().any(|f| f == "AF_UNIX"),
            "sd_notify 需要 AF_UNIX：{families}"
        );
    }

    #[test]
    fn capture_records_only_inside_capture() {
        attention("outside");
        let ((), events) = capture(|| {
            attention("a");
            set_status("b");
        });
        assert_eq!(
            events,
            [Event::Stderr("a".into()), Event::Status("b".into())]
        );
        set_status("outside");
    }
}
