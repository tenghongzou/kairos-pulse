//! 監控 task：每 1 秒 sweep、每 5 秒寫 agents.json、每 60 秒輸出摘要。
//! 收到停止通知時結束；關機時的最後一份 agents.json 由 [`write_final_snapshot`] 寫出
//! （由 main 呼叫，monitor 異常結束時也會寫出；R2-K17）。

use chrono::Utc;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::registry::Registry;
use crate::storage::{DataPaths, write_agents_snapshot};

const SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// 以 sweep 次數計算的週期
const SNAPSHOT_EVERY_TICKS: u64 = 5;
const SUMMARY_EVERY_TICKS: u64 = 60;

/// 快照持續寫入失敗時，重複記錄 ERROR 的間隔
const SNAPSHOT_FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// 每個時間窗最多逐一記錄幾個「從狀態表移除」的 agent，其餘在時間窗結束後以一行摘要記錄
/// （例如狀態表被灌入大量 agent 後 24 小時一起到期）
const MAX_REMOVED_LOGGED: usize = 5;

/// 每個時間窗最多逐一記錄幾個「標記為離線」的 agent，其餘在時間窗結束後以一行摘要記錄
/// （例如網路中斷時大量 agent 同時離線，或大量連線刻意保持沉默；R2-K12）
const MAX_WENT_OFFLINE_LOGGED: usize = 20;

/// 上面兩個上限的時間窗
const LOG_WINDOW: Duration = Duration::from_secs(10);

/// 每個時間窗最多逐一記錄 `limit` 行，超過的只計數，在時間窗結束後彙總成一行
#[derive(Debug)]
struct WindowedLines {
    limit: usize,
    window_start: Option<Instant>,
    used: usize,
    overflow: usize,
}

impl WindowedLines {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            window_start: None,
            used: 0,
            overflow: 0,
        }
    }

    /// 時間窗已結束時開始新的時間窗，回傳上一個時間窗未逐一記錄的數量（需輸出摘要時）
    fn roll(&mut self, now: Instant) -> Option<usize> {
        let expired = self
            .window_start
            .is_none_or(|start| now.saturating_duration_since(start) >= LOG_WINDOW);
        if !expired {
            return None;
        }
        self.window_start = Some(now);
        self.used = 0;
        let overflow = std::mem::take(&mut self.overflow);
        (overflow > 0).then_some(overflow)
    }

    /// 是否可以逐一記錄這一行；否則計入摘要
    fn admit(&mut self) -> bool {
        if self.used < self.limit {
            self.used += 1;
            true
        } else {
            self.overflow += 1;
            false
        }
    }
}

/// 快照寫入失敗的記錄狀態：第一次失敗立即記錄，持續失敗時每 60 秒再記一次（附失敗次數），恢復時記錄一次
#[derive(Debug, Default)]
struct FailureLog {
    failures: u64,
    last_logged: Option<Instant>,
}

impl FailureLog {
    /// 失敗時呼叫；回傳 `Some(連續失敗次數)` 表示應記錄
    fn failed(&mut self, now: Instant) -> Option<u64> {
        self.failures += 1;
        let due = self
            .last_logged
            .is_none_or(|t| now.saturating_duration_since(t) >= SNAPSHOT_FAILURE_LOG_INTERVAL);
        if due {
            self.last_logged = Some(now);
            Some(self.failures)
        } else {
            None
        }
    }

    /// 成功時呼叫；回傳 `true` 表示剛從失敗中恢復
    fn succeeded(&mut self) -> bool {
        let recovered = self.failures > 0;
        *self = Self::default();
        recovered
    }
}

pub async fn run(registry: Arc<Registry>, paths: DataPaths, stop: CancellationToken) {
    let mut ticker = interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tick: u64 = 0;
    // 快照寫入失敗時節流記錄，避免每 5 秒重複洗版，但持續失敗（例如 Windows 上 rename 一直失敗）仍會定期提醒
    let mut snapshot_failures = FailureLog::default();
    let mut offline_lines = WindowedLines::new(MAX_WENT_OFFLINE_LOGGED);
    let mut removed_lines = WindowedLines::new(MAX_REMOVED_LOGGED);

    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let now = Instant::now();
        let swept = registry.sweep(now);
        if let Some(n) = offline_lines.roll(now) {
            warn!(
                "過去 {} 秒內另有 {} 個 agent 超過離線門檻沒有合法訊息，標記為離線（未逐一記錄）",
                LOG_WINDOW.as_secs(),
                n
            );
        }
        for gone in &swept.went_offline {
            if offline_lines.admit() {
                warn!(
                    "agent {}（{}）已 {:.1} 秒沒有合法訊息，標記為離線",
                    gone.agent_id,
                    gone.hostname.escape_debug(),
                    gone.silent_for.as_secs_f64()
                );
            }
        }
        if let Some(n) = removed_lines.roll(now) {
            info!(
                "過去 {} 秒內另有 {} 個離線超過 24 小時的 agent 從狀態表移除（未逐一記錄）",
                LOG_WINDOW.as_secs(),
                n
            );
        }
        for removed in &swept.removed {
            if removed_lines.admit() {
                info!(
                    "agent {}（{}）已離線 {:.1} 小時，從狀態表移除",
                    removed.agent_id,
                    removed.hostname.escape_debug(),
                    removed.offline_for.as_secs_f64() / 3600.0
                );
            }
        }

        if tick.is_multiple_of(SNAPSHOT_EVERY_TICKS) {
            let snapshot = registry.snapshot(Utc::now());
            match write_agents_snapshot(&paths, &snapshot).await {
                Ok(()) => {
                    if snapshot_failures.succeeded() {
                        info!("{} 已恢復寫入", paths.agents.display());
                    }
                }
                Err(e) => {
                    if let Some(count) = snapshot_failures.failed(Instant::now()) {
                        error!("寫入 agent 快照失敗（連續 {} 次）: {}", count, e);
                    }
                }
            }
        }

        if tick > 0 && tick.is_multiple_of(SUMMARY_EVERY_TICKS) {
            let (online, total) = registry.counts();
            info!("摘要：線上 {} / 共 {} 個 agent", online, total);
        }

        tick = tick.wrapping_add(1);
    }
}

/// 關機：寫出最後的 agents.json（此時所有 agent 應已標為離線）
pub async fn write_final_snapshot(registry: &Registry, paths: &DataPaths) {
    let snapshot = registry.snapshot(Utc::now());
    match write_agents_snapshot(paths, &snapshot).await {
        Ok(()) => info!(
            "已寫入最後的 agent 快照 {}（線上 {} / 共 {}）",
            paths.agents.display(),
            snapshot.online,
            snapshot.total
        ),
        Err(e) => error!("寫入最後的 agent 快照失敗: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::HelloInfo;

    #[test]
    fn snapshot_failures_are_logged_periodically() {
        let mut f = FailureLog::default();
        let t0 = Instant::now();
        assert!(!f.succeeded());
        assert_eq!(f.failed(t0), Some(1));
        // 每 5 秒失敗一次：60 秒內不重複記錄
        for i in 1..12 {
            assert_eq!(f.failed(t0 + Duration::from_secs(5 * i)), None);
        }
        assert_eq!(f.failed(t0 + Duration::from_secs(60)), Some(13));
        assert!(f.succeeded());
        assert!(!f.succeeded());
        // 恢復後再失敗立即記錄
        assert_eq!(f.failed(t0 + Duration::from_secs(61)), Some(1));
    }

    #[test]
    fn sweep_log_lines_are_limited_per_window() {
        // R2-K12：每秒 sweep 都有大量 agent 離線時，每 10 秒最多逐一記錄 20 行，其餘彙總為一行
        let t0 = Instant::now();
        let mut w = WindowedLines::new(20);
        assert_eq!(w.roll(t0), None);
        let admitted = (0..50).filter(|_| w.admit()).count();
        assert_eq!(admitted, 20);
        // 同一個時間窗內的下一次 sweep：不輸出摘要、也不再逐一記錄
        assert_eq!(w.roll(t0 + Duration::from_secs(1)), None);
        assert!(!w.admit());
        // 時間窗結束：摘要為 31 個，之後重新可記錄
        assert_eq!(w.roll(t0 + LOG_WINDOW), Some(31));
        assert!(w.admit());
        assert_eq!(w.roll(t0 + LOG_WINDOW * 2), None);
    }

    #[tokio::test]
    async fn run_sweeps_writes_snapshots_and_final_snapshot_on_stop() {
        // T4：monitor::run 的 sweep、定期快照與停止時的最後快照
        let dir = std::env::temp_dir().join(format!(
            "kairos-monitor-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        let paths = DataPaths::new(&dir);
        let registry = Arc::new(Registry::new());
        let t0 = Instant::now();
        let _reg = registry
            .register(
                1,
                HelloInfo {
                    agent_id: "a".into(),
                    hostname: "h".into(),
                    agent_version: "1".into(),
                    proto: 1,
                    host: None,
                    peer: "p".into(),
                    offline_after: Duration::from_millis(100),
                    client_chain: None,
                },
                t0,
                Utc::now(),
            )
            .unwrap();
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(registry.clone(), paths.clone(), stop.clone()));

        // 第一次 tick 立即 sweep 並寫出快照（此時仍在線）
        let mut first = None;
        for _ in 0..100 {
            if let Ok(text) = tokio::fs::read_to_string(&paths.agents).await {
                first = Some(text);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let first: serde_json::Value = serde_json::from_str(&first.expect("應寫出快照")).unwrap();
        assert_eq!(first["total"], 1);

        // 下一次 sweep（1 秒後）判定離線
        let mut offline = false;
        for _ in 0..100 {
            if registry.counts() == (0, 1) {
                offline = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(offline, "超過 offline_after 應被 sweep 標為離線");

        stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        registry.mark_all_offline(Instant::now());
        write_final_snapshot(&registry, &paths).await;
        let last: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&paths.agents).await.unwrap()).unwrap();
        assert_eq!(last["online"], 0);
        assert_eq!(last["total"], 1);
        assert_eq!(last["agents"][0]["online"], false);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
