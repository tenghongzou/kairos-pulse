//! 關機訊號與關機期限。
//!
//! 第一次收到訊號時記下 T0 並取消 [`CancellationToken`]，各階段共用以 T0 起算的單一期限：
//!
//! | 階段 | 最晚完成 |
//! |---|---|
//! | 連線送出 close_notify 並結束 | T0 + 2.0 s |
//! | writer 寫完佇列 | T0 + 2.5 s |
//! | monitor 寫出最後的 agents.json | T0 + 3.0 s |
//! | 看門狗強制結束（exit 1） | T0 + 4.0 s |
//!
//! Windows 在關閉主控台或系統關機時約 5 秒後會強制終止行程，因此整個流程必須遠低於 5 秒。
//! 第二次收到訊號時直接以 130 結束行程。
//!
//! 背景工作（report writer、monitor、TLS 重載）異常結束時，以 [`fatal`] 走同一套關機流程，
//! 並在最後以非 0 的結束代碼結束，讓服務管理程式能發現並重新啟動。
//!
//! - Windows：Ctrl+C、Ctrl+Break、關閉主控台視窗、系統關機
//! - 其他平台：SIGINT、SIGTERM

use log::{error, info, warn};
use std::io::Error;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// 第二次收到關機訊號時的結束代碼（慣例為 128 + SIGINT）
pub const FORCED_EXIT_CODE: i32 = 130;
/// 看門狗觸發時的結束代碼
pub const WATCHDOG_EXIT_CODE: i32 = 1;

/// 連線收尾的期限（自 T0 起算）
pub const CONNECTIONS_DEADLINE: Duration = Duration::from_millis(2000);
/// writer 寫完佇列的期限
pub const WRITER_DEADLINE: Duration = Duration::from_millis(2500);
/// monitor 寫出最後快照的期限
pub const MONITOR_DEADLINE: Duration = Duration::from_millis(3000);
/// 看門狗：超過此時間行程仍在就強制結束
pub const WATCHDOG_DEADLINE: Duration = Duration::from_millis(4000);
/// 關機期間，送出 close_notify 後讀取並丟棄對方資料的上限
pub const SHUTDOWN_DRAIN_LIMIT: Duration = Duration::from_millis(200);

/// 關機狀態：取消訊號與 T0
#[derive(Debug, Default)]
pub struct Shutdown {
    token: CancellationToken,
    started: OnceLock<Instant>,
    /// 因內部錯誤而關機時的原因（只記第一個）
    fatal: OnceLock<String>,
}

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// 開始關機：記下 T0（只記第一次）並取消 token。回傳 T0。
    pub fn trigger(&self) -> Instant {
        let t0 = *self.started.get_or_init(Instant::now);
        self.token.cancel();
        t0
    }

    /// 等待關機開始
    pub async fn cancelled(&self) {
        self.token.cancelled().await
    }

    /// 關機開始的時間（尚未關機時為 `None`）
    pub fn started_at(&self) -> Option<Instant> {
        self.started.get().copied()
    }

    /// 因內部錯誤開始關機：記下原因（只記第一個）並觸發關機。回傳 T0。
    pub fn trigger_fatal(&self, reason: impl Into<String>) -> Instant {
        let _ = self.fatal.set(reason.into());
        self.trigger()
    }

    /// 因內部錯誤而關機時的原因；正常關機（收到訊號）時為 `None`
    pub fn fatal_reason(&self) -> Option<&str> {
        self.fatal.get().map(String::as_str)
    }
}

/// 背景工作異常時呼叫：記錄 ERROR、開始關機，並啟動看門狗確保行程會結束
pub fn fatal(shutdown: &Shutdown, reason: String) {
    error!("{}，開始關機", reason);
    let first = shutdown.started_at().is_none();
    let t0 = shutdown.trigger_fatal(reason);
    if first {
        start_watchdog(t0);
    }
}

/// `Conn::close()` 讀取丟棄對方資料的時間上限：
/// 平常為 `normal`；關機期間為 200 ms，且不得超過連線收尾期限的剩餘時間
pub fn drain_limit(started: Option<Instant>, now: Instant, normal: Duration) -> Duration {
    match started {
        None => normal,
        Some(t0) => {
            let remaining = (t0 + CONNECTIONS_DEADLINE).saturating_duration_since(now);
            SHUTDOWN_DRAIN_LIMIT.min(remaining).min(normal)
        }
    }
}

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
        let ctx =
            |what: &str, e: Error| Error::new(e.kind(), format!("無法註冊 {} 訊號: {}", what, e));
        Ok(Self {
            ctrl_c: windows::ctrl_c().map_err(|e| ctx("Ctrl+C", e))?,
            ctrl_break: windows::ctrl_break().map_err(|e| ctx("Ctrl+Break", e))?,
            ctrl_close: windows::ctrl_close().map_err(|e| ctx("主控台關閉", e))?,
            ctrl_shutdown: windows::ctrl_shutdown().map_err(|e| ctx("系統關機", e))?,
        })
    }

    /// 等待下一個關機訊號，回傳訊號名稱
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
        let ctx =
            |what: &str, e: Error| Error::new(e.kind(), format!("無法註冊 {} 訊號: {}", what, e));
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).map_err(|e| ctx("SIGINT", e))?,
            terminate: signal(SignalKind::terminate()).map_err(|e| ctx("SIGTERM", e))?,
        })
    }

    /// 等待下一個關機訊號，回傳訊號名稱
    pub async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

/// 看門狗：在獨立的 OS 執行緒等到 T0 + 4 秒，若行程仍未結束就強制結束。
/// 用來防止 fs 卡住等無法取消的阻塞讓關機超過 Windows 給的時間。
fn start_watchdog(t0: Instant) {
    let spawned = std::thread::Builder::new()
        .name("kairos-shutdown-watchdog".into())
        .spawn(move || {
            std::thread::sleep((t0 + WATCHDOG_DEADLINE).saturating_duration_since(Instant::now()));
            error!(
                "關機超過 {:.1} 秒仍未完成，強制結束",
                WATCHDOG_DEADLINE.as_secs_f64()
            );
            log::logger().flush();
            std::process::exit(WATCHDOG_EXIT_CODE);
        });
    if let Err(e) = spawned {
        warn!("無法啟動關機看門狗: {}", e);
    }
}

/// 監聽關機訊號：第一次開始關機並啟動看門狗，第二次直接結束行程
pub async fn watch(mut signals: Signals, shutdown: std::sync::Arc<Shutdown>) {
    let first = signals.recv().await;
    let t0 = shutdown.trigger();
    start_watchdog(t0);
    info!("收到 {} 訊號，開始關機（再送一次訊號可強制結束）", first);

    let second = signals.recv().await;
    warn!("再次收到 {} 訊號，強制結束", second);
    log::logger().flush();
    std::process::exit(FORCED_EXIT_CODE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_records_first_time_only() {
        let s = Shutdown::new();
        assert_eq!(s.started_at(), None);
        let t0 = s.trigger();
        assert_eq!(s.started_at(), Some(t0));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(s.trigger(), t0);
    }

    #[test]
    fn fatal_reason_is_recorded_once() {
        let s = Shutdown::new();
        assert_eq!(s.fatal_reason(), None);
        let t0 = s.trigger_fatal("writer 異常結束");
        assert_eq!(s.started_at(), Some(t0));
        assert_eq!(s.trigger_fatal("monitor 異常結束"), t0);
        assert_eq!(s.fatal_reason(), Some("writer 異常結束"));

        // 收到訊號的正常關機沒有原因
        let s = Shutdown::new();
        s.trigger();
        assert_eq!(s.fatal_reason(), None);
    }

    #[test]
    fn deadlines_are_ordered_and_below_windows_limit() {
        assert!(CONNECTIONS_DEADLINE < WRITER_DEADLINE);
        assert!(WRITER_DEADLINE < MONITOR_DEADLINE);
        assert!(MONITOR_DEADLINE < WATCHDOG_DEADLINE);
        assert!(WATCHDOG_DEADLINE < Duration::from_secs(5));
    }

    #[test]
    fn drain_limit_during_shutdown() {
        let normal = Duration::from_secs(1);
        let t0 = Instant::now();
        // 平常
        assert_eq!(drain_limit(None, t0, normal), normal);
        // 關機剛開始：200 ms
        assert_eq!(drain_limit(Some(t0), t0, normal), SHUTDOWN_DRAIN_LIMIT);
        // 剩 50 ms：不得超過剩餘期限
        let near = t0 + CONNECTIONS_DEADLINE - Duration::from_millis(50);
        assert_eq!(
            drain_limit(Some(t0), near, normal),
            Duration::from_millis(50)
        );
        // 已過期限：0
        let late = t0 + CONNECTIONS_DEADLINE + Duration::from_millis(1);
        assert_eq!(drain_limit(Some(t0), late, normal), Duration::ZERO);
    }

    #[tokio::test]
    async fn cancelled_resolves_after_trigger() {
        let s = std::sync::Arc::new(Shutdown::new());
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.cancelled().await });
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }
}
