//! 監控 task：每 1 秒 sweep、每 5 秒寫 agents.json、每 60 秒輸出摘要。
//! 收到停止通知時最後寫一次 agents.json 再結束。

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

pub async fn run(registry: Arc<Registry>, paths: DataPaths, stop: CancellationToken) {
    let mut ticker = interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tick: u64 = 0;
    // 快照寫入失敗時只在狀態改變時記錄，避免每 5 秒重複洗版
    let mut snapshot_failing = false;

    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let swept = registry.sweep(Instant::now());
        for gone in swept.went_offline {
            warn!(
                "agent {}（{}）已 {:.1} 秒沒有合法訊息，標記為離線",
                gone.agent_id,
                gone.hostname.escape_debug(),
                gone.silent_for.as_secs_f64()
            );
        }
        for removed in swept.removed {
            info!(
                "agent {}（{}）已離線 {:.1} 小時，從狀態表移除",
                removed.agent_id,
                removed.hostname.escape_debug(),
                removed.offline_for.as_secs_f64() / 3600.0
            );
        }

        if tick.is_multiple_of(SNAPSHOT_EVERY_TICKS) {
            let snapshot = registry.snapshot(Utc::now());
            match write_agents_snapshot(&paths, &snapshot).await {
                Ok(()) => {
                    if snapshot_failing {
                        info!("{} 已恢復寫入", paths.agents.display());
                        snapshot_failing = false;
                    }
                }
                Err(e) => {
                    if !snapshot_failing {
                        error!("寫入 agent 快照失敗: {}", e);
                        snapshot_failing = true;
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

    // 關機：寫出最後的狀態（此時所有 agent 應已標為離線）
    let snapshot = registry.snapshot(Utc::now());
    match write_agents_snapshot(&paths, &snapshot).await {
        Ok(()) => info!(
            "已寫入最後的 agent 快照 {}（線上 {} / 共 {}）",
            paths.agents.display(),
            snapshot.online,
            snapshot.total
        ),
        Err(e) => error!("寫入最後的 agent 快照失敗: {}", e),
    }
}
