//! 終端機的色彩與符號（`design-system/cli-style.md` §3、§4；`pulse/cli-spec.md` §1.1）。

use std::io::IsTerminal as _;

/// `--color`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

/// 單一輸出串流的呈現方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Unicode 符號（`✓`）或 ASCII 標籤（`[ OK ]`）
    pub unicode: bool,
    /// ANSI 16 色前景色
    pub color: bool,
}

impl Style {
    /// 純文字（非 TTY、`NO_COLOR`）：ASCII 標籤、不上色
    #[cfg(test)]
    pub const PLAIN: Style = Style {
        unicode: false,
        color: false,
    };

    /// 以 ANSI 代碼包住 `text`（不上色時原樣）
    pub fn paint(&self, code: &str, text: &str) -> String {
        if self.color && !code.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// 串流
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// 依旗標、環境變數與串流決定呈現方式（stdout 與 stderr 分開判斷）
pub fn detect(stream: Stream, color: ColorChoice, ascii: bool, json: bool) -> Style {
    let tty = match stream {
        Stream::Stdout => std::io::stdout().is_terminal(),
        Stream::Stderr => std::io::stderr().is_terminal(),
    };
    let env = |k: &str| std::env::var(k).ok();
    Style {
        unicode: decide_unicode(ascii, env("PULSE_SYMBOLS").as_deref(), tty, &env),
        color: decide_color(color, json, tty, &env) && enable_vt(stream),
    }
}

/// `cli-style.md` §3.1 的判斷順序
fn decide_color(
    choice: ColorChoice,
    json: bool,
    tty: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    match choice {
        ColorChoice::Always => return true,
        ColorChoice::Never => return false,
        ColorChoice::Auto => {}
    }
    if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if env("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    if json || !tty {
        return false;
    }
    if env("TERM").as_deref() == Some("dumb") {
        return false;
    }
    true
}

/// `cli-style.md` §4.2 的判斷順序
fn decide_unicode(
    ascii: bool,
    symbols: Option<&str>,
    tty: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    if ascii {
        return false;
    }
    match symbols.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("ascii") => return false,
        Some(s) if s.eq_ignore_ascii_case("unicode") => return true,
        _ => {}
    }
    if !tty {
        return false;
    }
    if cfg!(windows) {
        return env("WT_SESSION").is_some()
            || env("TERM_PROGRAM").as_deref() == Some("vscode")
            || env("ConEmuANSI").as_deref() == Some("ON")
            || env("TERM").is_some();
    }
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| env(k).filter(|v| !v.is_empty()))
        .is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        })
}

/// Windows：開啟主控台的 ANSI 跳脫序列處理（`ENABLE_VIRTUAL_TERMINAL_PROCESSING`）；
/// 失敗（舊版主控台、不是主控台）時不上色。其他平台一律可用
#[cfg(windows)]
fn enable_vt(stream: Stream) -> bool {
    use std::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    }
    let which = match stream {
        Stream::Stdout => STD_OUTPUT_HANDLE,
        Stream::Stderr => STD_ERROR_HANDLE,
    };
    // SAFETY: 只查詢與設定目前行程的主控台模式；handle 由 GetStdHandle 取得，mode 指向區域變數
    unsafe {
        let handle = GetStdHandle(which);
        if handle.is_null() || handle as isize == -1 {
            return false;
        }
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_vt(_stream: Stream) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn color_rules_follow_cli_style() {
        let none = env_of(&[]);
        assert!(decide_color(ColorChoice::Auto, false, true, &none));
        assert!(
            !decide_color(ColorChoice::Auto, false, false, &none),
            "非 TTY"
        );
        assert!(
            !decide_color(ColorChoice::Auto, true, true, &none),
            "--json"
        );
        assert!(decide_color(ColorChoice::Always, true, false, &none));
        assert!(!decide_color(ColorChoice::Never, false, true, &none));
        assert!(!decide_color(
            ColorChoice::Auto,
            false,
            true,
            &env_of(&[("NO_COLOR", "1")])
        ));
        // NO_COLOR 為空字串時不算
        assert!(decide_color(
            ColorChoice::Auto,
            false,
            true,
            &env_of(&[("NO_COLOR", "")])
        ));
        assert!(decide_color(
            ColorChoice::Auto,
            false,
            false,
            &env_of(&[("CLICOLOR_FORCE", "1")])
        ));
        assert!(!decide_color(
            ColorChoice::Auto,
            false,
            false,
            &env_of(&[("CLICOLOR_FORCE", "0")])
        ));
        assert!(!decide_color(
            ColorChoice::Auto,
            false,
            true,
            &env_of(&[("TERM", "dumb")])
        ));
    }

    #[test]
    fn symbol_rules_follow_cli_style() {
        let utf8 = env_of(&[
            ("LANG", "zh_TW.UTF-8"),
            ("TERM", "xterm"),
            ("WT_SESSION", "x"),
        ]);
        assert!(!decide_unicode(true, None, true, &utf8), "--ascii");
        assert!(!decide_unicode(false, Some("ascii"), true, &utf8));
        assert!(decide_unicode(false, Some("UNICODE"), false, &utf8));
        assert!(!decide_unicode(false, None, false, &utf8), "非 TTY");
        assert!(decide_unicode(false, None, true, &utf8));
        if !cfg!(windows) {
            assert!(!decide_unicode(
                false,
                None,
                true,
                &env_of(&[("LANG", "C")])
            ));
            assert!(decide_unicode(
                false,
                None,
                true,
                &env_of(&[("LC_ALL", "en_US.utf8"), ("LANG", "C")])
            ));
        } else {
            // 傳統主控台（沒有 WT_SESSION、TERM 等）一律 ASCII
            assert!(!decide_unicode(false, None, true, &env_of(&[])));
        }
    }

    #[test]
    fn paint_only_when_color_is_on() {
        let on = Style {
            unicode: true,
            color: true,
        };
        assert_eq!(on.paint("32", "線上"), "\x1b[32m線上\x1b[0m");
        assert_eq!(Style::PLAIN.paint("32", "線上"), "線上");
        assert_eq!(on.paint("", "x"), "x");
    }
}
