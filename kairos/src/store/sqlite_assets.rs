//! SQLite 後端的主機資產（`inventory` 訊息）寫入（`crates/kairos-proto/METRICS.md` 第 9.3 節）：
//! 與上一次合併（[`crate::store::assets::merge`]）、摘要改變時寫入變更歷史、更新開機時間。
//! 一則一個交易，由 writer 在 report 的批次之間處理（[`crate::store::run_store_writer_with_assets`]）。

use super::*;
use crate::store::assets::{self, Change, MAX_CHANGES_PER_HOST};
use crate::store::{AssetOutcome, AssetWrite};
use kairos_proto::{Inventory, inventory_digest};

impl SqliteStore {
    /// 保存一則資產；失敗時回傳原因（連線回 `storage_unavailable`）
    pub(super) fn store_assets(&mut self, w: &AssetWrite) -> Result<AssetOutcome, String> {
        self.ensure_ready()?;
        match self.assets_tx(w) {
            Ok(Some(outcome)) => Ok(outcome),
            Ok(None) => Err(HOSTS_FULL.to_string()),
            Err(e) => {
                let reason = format!("寫入 {} 的主機資產失敗: {}", self.opts.path.display(), e);
                throttled!(
                    self.throttle,
                    Category::StorageWrite,
                    error,
                    "{}（agent {}）",
                    reason,
                    w.agent_id
                );
                self.invalidate(is_connection_fatal(&e));
                Err(reason)
            }
        }
    }

    fn assets_tx(&mut self, w: &AssetWrite) -> rusqlite::Result<Option<AssetOutcome>> {
        let Self {
            conn, caches, opts, ..
        } = self;
        let Some(conn) = conn.as_mut() else {
            return Err(rusqlite::Error::InvalidQuery);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(host_id) =
            caches.host_id_of(&tx, &w.agent_id, &w.hostname, &w.peer, w.received_ms, opts)?
        else {
            return Ok(None);
        };
        let prev: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT digest, body, changed_at FROM host_inventory WHERE host_id = ?1",
                [host_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let prev_inv: Option<Inventory> = prev
            .as_ref()
            .and_then(|(_, body, _)| serde_json::from_str(body).ok());
        let merged = assets::merge(prev_inv.as_ref(), &w.inventory);
        let digest = inventory_digest(&merged);
        let first = prev.is_none();
        let changed = prev.as_ref().is_some_and(|(d, _, _)| *d != digest);
        let mut history = 0;
        if changed && let Some(before) = &prev_inv {
            let changes: Vec<Change> = assets::diff(before, &merged);
            if !changes.is_empty() {
                tx.execute(
                    "INSERT INTO inventory_changes (host_id, changed_at, changes) VALUES (?1, ?2, ?3)",
                    params![
                        host_id,
                        w.collected_ms,
                        serde_json::to_string(&changes).unwrap_or_else(|_| "[]".into())
                    ],
                )?;
                tx.execute(
                    "DELETE FROM inventory_changes WHERE host_id = ?1 AND id NOT IN
                     (SELECT id FROM inventory_changes WHERE host_id = ?1 ORDER BY id DESC LIMIT ?2)",
                    params![host_id, MAX_CHANGES_PER_HOST],
                )?;
                history = 1;
            }
        }
        let changed_at = match &prev {
            Some((_, _, at)) if !changed => *at,
            _ => w.collected_ms,
        };
        let os = merged.os.as_ref();
        let cpu = merged.cpu.as_ref();
        let sys = merged.system.as_ref();
        let logical_cores = cpu.and_then(|c| c.logical_cores);
        let os_family = os.and_then(|o| o.family.clone());
        tx.execute(
            "INSERT INTO host_inventory (host_id, digest, body, collected_at, received_at, changed_at,
                os_family, cpu_model, cpu_logical_cores, memory_total, vendor, product, serial)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT (host_id) DO UPDATE SET
                digest = excluded.digest, body = excluded.body, collected_at = excluded.collected_at,
                received_at = excluded.received_at, changed_at = excluded.changed_at,
                os_family = excluded.os_family, cpu_model = excluded.cpu_model,
                cpu_logical_cores = excluded.cpu_logical_cores, memory_total = excluded.memory_total,
                vendor = excluded.vendor, product = excluded.product, serial = excluded.serial",
            params![
                host_id,
                digest,
                serde_json::to_string(&merged).unwrap_or_else(|_| "{}".into()),
                w.collected_ms,
                w.received_ms,
                changed_at,
                os_family,
                cpu.and_then(|c| c.model.clone()),
                logical_cores,
                merged
                    .memory
                    .as_ref()
                    .and_then(|m| m.total)
                    .and_then(|t| i64::try_from(t).ok()),
                sys.and_then(|s| s.vendor.clone()),
                sys.and_then(|s| s.product.clone()),
                sys.and_then(|s| s.serial.clone()),
            ],
        )?;
        if let Some(boot) = assets::boot_time_secs(&w.inventory) {
            tx.execute(
                "UPDATE hosts SET boot_time = ?2 WHERE host_id = ?1",
                params![host_id, boot],
            )?;
        }
        tx.commit()?;
        if first {
            debug!("首次收到主機 {} 的資產（{}）", w.agent_id, digest);
        } else if changed {
            debug!(
                "主機 {} 的資產已改變（{}；變更歷史 {} 筆）",
                w.agent_id, digest, history
            );
        }
        Ok(Some(AssetOutcome {
            first,
            changed,
            history,
            logical_cores,
            os_family,
        }))
    }
}
