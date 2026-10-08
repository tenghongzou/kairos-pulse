//! 結束訊號處理。
//!
//! - Windows：Ctrl+C、Ctrl+Break、關閉主控台視窗、系統關機
//! - 其他平台：SIGINT、SIGTERM
//!
//! 只監聽 Ctrl+C 的話，關閉主控台視窗或系統關機時行程會被直接終止，
//! 不會送出 TLS close_notify。

use std::io::Error;

/// 第二次收到結束訊號時的結束代碼（慣例為 128 + SIGINT）
pub const FORCED_EXIT_CODE: i32 = 130;

#[cfg(windows)]
pub struct Signals {
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_break: tokio::signal::windows::CtrlBreak,
    ctrl_close: tokio::signal::windows::CtrlClose,
    ctrl_shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl Signals {
    pub fn new() -> Result<Self, Error> {
        use tokio::signal::windows;
        let ctx = |what: &str, e: Error| Error::new(e.kind(), format!("無法監聽 {what} 訊號：{e}"));
        Ok(Self {
            ctrl_c: windows::ctrl_c().map_err(|e| ctx("Ctrl+C", e))?,
            ctrl_break: windows::ctrl_break().map_err(|e| ctx("Ctrl+Break", e))?,
            ctrl_close: windows::ctrl_close().map_err(|e| ctx("主控台關閉", e))?,
            ctrl_shutdown: windows::ctrl_shutdown().map_err(|e| ctx("系統關機", e))?,
        })
    }

    /// 等待下一個結束訊號，回傳訊號名稱
    pub async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.ctrl_c.recv() => "Ctrl+C",
            _ = self.ctrl_break.recv() => "Ctrl+Break",
            _ = self.ctrl_close.recv() => "主控台關閉",
            _ = self.ctrl_shutdown.recv() => "系統關機",
        }
    }
}

#[cfg(unix)]
pub struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    pub fn new() -> Result<Self, Error> {
        use tokio::signal::unix::{SignalKind, signal};
        let ctx = |what: &str, e: Error| Error::new(e.kind(), format!("無法監聽 {what} 訊號：{e}"));
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).map_err(|e| ctx("SIGINT", e))?,
            terminate: signal(SignalKind::terminate()).map_err(|e| ctx("SIGTERM", e))?,
        })
    }

    /// 等待下一個結束訊號，回傳訊號名稱
    pub async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}
