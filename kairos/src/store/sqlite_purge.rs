//! SQLite 後端的「刪除主機資料」（[`crate::store::purge`]）：writer 在 report 的批次之間逐步執行，
//! 第一步清除這台主機的快取（主機編號、序列、計數器的上一筆、boot）與記憶體中尚未寫出的彙總；
//! 執行期間與完成後 [`PURGE_HOLD`] 內不寫入它的 report、資產與清冊同步，避免被建立回來。

use super::*;
use crate::store::purge::{self, PurgeJob};

/// 刪除開始後多久之內拒絕這台主機的寫入（狀態表的快照每 5 秒一份、清冊每 30 秒同步一次；
/// 主機的憑證已撤銷，被撤銷的連線在 CRL 生效後斷線）
pub(super) const PURGE_HOLD: Duration = Duration::from_secs(10 * 60);

/// 寫入被拒的原因（連線回 `storage_unavailable`）
pub(super) const PURGING: &str = "這台主機的資料正在刪除（或剛刪除），不接受它的資料";

impl SqliteStore {
    /// 正在刪除或剛刪除的主機的 report 標為失敗
    pub(super) fn reject_purging(
        &mut self,
        records: &[ReportRecord],
        statuses: &mut [WriteStatus],
    ) {
        if self.purging.is_empty() {
            return;
        }
        self.expire_purging();
        for (r, s) in records.iter().zip(statuses.iter_mut()) {
            if self.purging.contains_key(&r.agent_id) {
                *s = WriteStatus::Failed(PURGING.to_string());
            }
        }
    }

    pub(super) fn expire_purging(&mut self) {
        self.purging.retain(|_, t| t.elapsed() < PURGE_HOLD);
    }

    /// 做一步刪除；回傳是否完成。失敗時回傳原因（已完成的步驟保留，重新執行即可接續）
    pub(super) fn purge(&mut self, job: &mut PurgeJob) -> Result<bool, String> {
        self.ensure_ready()?;
        if !job.started {
            self.purging.insert(job.agent_id.clone(), Instant::now());
            let host_id = match self.caches.hosts.remove(&job.agent_id) {
                Some(e) => Some(e.host_id),
                None => {
                    let conn = self.conn.as_ref().ok_or("資料庫未開啟")?;
                    purge::host_id_of(conn, &job.agent_id).map_err(|e| e.to_string())?
                }
            };
            if let Some(h) = host_id {
                self.caches.host_series.remove(&h);
                self.caches.counters.remove(&h);
                self.caches.seeded.remove(&h);
                self.caches.boots.remove(&h);
                job.counts.rollup_pending += self.rollup.remove_host(h) as u64;
            }
            self.pending_inventory
                .retain(|r| r.agent_id != job.agent_id);
            self.series_cap_logged.remove(&job.agent_id);
        }
        let now_ms = (self.clock)().timestamp_millis();
        let Some(conn) = self.conn.as_mut() else {
            return Err("資料庫未開啟".into());
        };
        match purge::step(conn, job, DELETE_BATCH_ROWS, now_ms) {
            Ok(done) => {
                if done {
                    // 保留清除的進度中可能有這台主機（刪除不存在的主機的資料沒有影響），boot 快取清空
                    self.caches.boots.clear();
                }
                Ok(done)
            }
            Err(e) => {
                let reason = format!(
                    "刪除主機 {} 的資料時資料庫錯誤（已完成的部分保留，重新執行可接續）: {e}",
                    job.agent_id
                );
                self.invalidate(is_connection_fatal(&e));
                Err(reason)
            }
        }
    }
}
