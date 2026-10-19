//! [`crate::store::hosts`] 的 SQLite 實作（[`HostStore`]）。SQL 原封不動自呼叫端搬來。

#[allow(unused_imports)]
use rusqlite::{Connection, OptionalExtension, params};

use super::conn::{AsConn, forward};
#[allow(unused_imports)]
use crate::store::hosts::*;

impl<C: AsConn> HostStore for C {
    forward! {
        fn db_page_stats(&self) -> (i64, i64) => db_page_stats;
        fn host_id_of(&self, agent_id: &str) -> Option<i64> => host_id_of;
        fn trend_host(&self, agent_id: &str) -> Option<(i64, i64, Option<i64>)> => trend_host;
        fn host_series(&self, host_id: i64, name: &str) -> Vec<(i64, String, String, i64)> => host_series_of;
        fn standard_series_id(&self, name: &str, unit: &str) -> Option<i64> => standard_series_id;
        fn uptime_series_id(&self, name: &str) -> Option<i64> => uptime_series_id;
        fn series_info(&self, series_id: i64) -> Option<(String, String, String, i64)> => series_info;
        fn report_ts_before(&self, host_id: i64, before_ms: i64) -> Option<i64> => report_ts_before;
        fn latest_report(&self, host_id: i64) -> Option<(i64, i64, Vec<u8>)> => latest_report;
        fn for_each_report(&self, host_id: i64, from_ms: i64, to_ms: i64, f: &mut dyn FnMut(i64, i64, &[u8])) -> () => for_each_report;
        fn for_each_rollup(&self, table: &str, host_id: i64, from_bucket: i64, to_bucket: i64, wanted: &dyn Fn(i64) -> bool, f: &mut dyn FnMut(i64, i64, crate::store::rollup::Agg)) -> () => for_each_rollup;
        fn recent_rollup_series(&self, host_id: i64, since_bucket: i64) -> Vec<i64> => recent_rollup_series;
        fn host_group_of(&self, agent_id: &str) -> Option<Option<String>> => host_group_of;
        fn host_basics(&self, agent_id: &str) -> Option<(i64, Option<String>, Option<i64>)> => host_basics;
        fn host_inventory(&self, host_id: i64) -> Option<(String, String, i64, i64, i64)> => host_inventory;
        fn inventory_changes(&self, host_id: i64, before: i64, limit: i64) -> Vec<(i64, i64, String)> => inventory_changes;
        fn process_snapshot_at(&self, host_id: i64, at_ms: i64) -> Option<(i64, Vec<u8>)> => process_snapshot_at;
        fn all_agent_ids(&self) -> Vec<String> => all_agent_ids;
        fn store_cert_rejections(&self, rows: &[(String, i64, String, Option<String>, Option<String>, i64)]) -> Vec<usize> => store_cert_rejections;
        fn host_names(&self, agent_id: &str) -> Option<(String, Option<String>)> => host_names;
        fn purge_plan(&self, agent_id: &str, now_ms: i64) -> crate::store::purge::PurgeCounts => crate::store::purge::plan;
    }
}

/// 資料庫的頁大小與使用中的頁數（原本在 `api/health.rs`）
pub fn db_page_stats(conn: &Connection) -> rusqlite::Result<(i64, i64)> {
    let pragma = |name: &str| conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0));
    let page_size = pragma("page_size")?;
    let pages = pragma("page_count")? - pragma("freelist_count")?;
    Ok((page_size, pages))
}

/// 主機的 host_id（原本在 `api/metrics.rs`）
pub fn host_id_of(conn: &Connection, agent_id: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT host_id FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()
}

/// 指標趨勢用的主機資訊（原本在 `api/metrics.rs` 的 `run_query`）
pub fn trend_host(
    conn: &Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<(i64, i64, Option<i64>)>> {
    conn.query_row(
        "SELECT host_id, rollup_until, report_interval_ms FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        },
    )
    .optional()
}

/// 這台主機這個名稱的序列：(series_id, unit, labels, derived)。v8 起以 `host_series` 只列出這台主機
/// 用過的序列；沒有對應時（例如 v7 升級時只出現在原始資料、尚未彙總的序列）改列出這個名稱的所有序列
pub fn host_series_of(
    conn: &Connection,
    host_id: i64,
    name: &str,
) -> rusqlite::Result<Vec<(i64, String, String, i64)>> {
    let collect = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<Vec<_>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    };
    let list = collect(
        "SELECT s.series_id, s.unit, s.labels, s.derived FROM series s
         JOIN host_series h USING (series_id) WHERE h.host_id = ?1 AND s.name = ?2",
        &[&host_id, &name],
    )?;
    if !list.is_empty() {
        return Ok(list);
    }
    collect(
        "SELECT series_id, unit, labels, derived FROM series WHERE name = ?1",
        &[&name],
    )
}

/// 標準序列的 series_id（原本在 `api/metrics.rs`）
pub fn standard_series_id(
    conn: &Connection,
    name: &str,
    unit: &str,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = '' AND derived = 0",
        params![name, unit],
        |r| r.get(0),
    )
    .optional()
}

/// 開機時間序列的 series_id（原本在 `api/metrics.rs`）
pub fn uptime_series_id(conn: &Connection, name: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT series_id FROM series WHERE name = ?1 AND unit = 's' AND labels = '' AND derived = 0",
        [name],
        |r| r.get(0),
    )
    .optional()
}

/// 序列的名稱、單位、label 與推算種類（原本在 `api/metrics.rs` 的 `available`）
pub fn series_info(
    conn: &Connection,
    series_id: i64,
) -> rusqlite::Result<Option<(String, String, String, i64)>> {
    let mut stmt =
        conn.prepare("SELECT name, unit, labels, derived FROM series WHERE series_id = ?1")?;
    stmt.query_row([series_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })
    .optional()
}

/// 之前最近一筆 report 的取樣時間（原本在 `api/metrics.rs`）
pub fn report_ts_before(
    conn: &Connection,
    host_id: i64,
    before_ms: i64,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT ts FROM reports WHERE host_id = ?1 AND ts < ?2 ORDER BY ts DESC LIMIT 1",
        params![host_id, before_ms],
        |r| r.get(0),
    )
    .optional()
}

/// 最新的一筆 report（原本在 `api/metrics.rs` 的 `available`）
pub fn latest_report(
    conn: &Connection,
    host_id: i64,
) -> rusqlite::Result<Option<(i64, i64, Vec<u8>)>> {
    conn.query_row(
        "SELECT ts, format, body FROM reports WHERE host_id = ?1 ORDER BY ts DESC LIMIT 1",
        [host_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

/// 逐筆讀取原始 report（原本在 `api/metrics.rs` 的 `read_reports`）
pub fn for_each_report(
    conn: &Connection,
    host_id: i64,
    from_ms: i64,
    to_ms: i64,
    f: &mut dyn FnMut(i64, i64, &[u8]),
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT ts, format, body FROM reports WHERE host_id = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts",
    )?;
    let mut rows = stmt.query(params![host_id, from_ms, to_ms])?;
    while let Some(r) = rows.next()? {
        let t: i64 = r.get(0)?;
        let format: i64 = r.get(1)?;
        let rusqlite::types::ValueRef::Blob(body) = r.get_ref(2)? else {
            continue;
        };
        f(t, format, body);
    }
    Ok(())
}

/// 逐列讀取彙總（原本在 `api/metrics.rs` 的 `run_query`）。`table` 只能是 `rollup_5m` 或 `rollup_1h`
pub fn for_each_rollup(
    conn: &Connection,
    table: &str,
    host_id: i64,
    from_bucket: i64,
    to_bucket: i64,
    wanted: &dyn Fn(i64) -> bool,
    f: &mut dyn FnMut(i64, i64, crate::store::rollup::Agg),
) -> rusqlite::Result<()> {
    debug_assert!(table == "rollup_5m" || table == "rollup_1h");
    let mut stmt = conn.prepare(&format!(
        "SELECT bucket, series_id, vmin, vmax, vsum, cnt FROM {table}
         WHERE host_id = ?1 AND bucket >= ?2 AND bucket < ?3"
    ))?;
    let mut rows = stmt.query(params![host_id, from_bucket, to_bucket])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(1)?;
        if !wanted(id) {
            continue;
        }
        let agg = crate::store::rollup::Agg {
            min: r.get(2)?,
            max: r.get(3)?,
            sum: r.get(4)?,
            count: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
        };
        let bucket: i64 = r.get(0)?;
        f(bucket, id, agg);
    }
    Ok(())
}

/// 最近的 1 小時彙總中的序列（原本在 `api/metrics.rs` 的 `available`）
pub fn recent_rollup_series(
    conn: &Connection,
    host_id: i64,
    since_bucket: i64,
) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT series_id FROM rollup_1h WHERE host_id = ?1 AND bucket >= ?2")?;
    let rows = stmt.query_map(params![host_id, since_bucket], |r| r.get(0))?;
    rows.collect::<rusqlite::Result<_>>()
}

/// 主機的群組（原本在 `api/hosts.rs`）
pub fn host_group_of(
    conn: &Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<Option<String>>> {
    conn.query_row(
        "SELECT host_group FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
}

/// 主機編號、`os`、開機時間（原本在 `api/assets.rs`）
pub fn host_basics(
    conn: &Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<(i64, Option<String>, Option<i64>)>> {
    conn.query_row(
        "SELECT host_id, os, boot_time FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

/// 主機目前的資產（原本在 `api/assets.rs`）
pub fn host_inventory(
    conn: &Connection,
    host_id: i64,
) -> rusqlite::Result<Option<(String, String, i64, i64, i64)>> {
    conn.query_row(
        "SELECT digest, body, collected_at, received_at, changed_at
         FROM host_inventory WHERE host_id = ?1",
        [host_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .optional()
}

/// 資產的變更歷史（原本在 `api/assets.rs`）
pub fn inventory_changes(
    conn: &Connection,
    host_id: i64,
    before: i64,
    limit: i64,
) -> rusqlite::Result<Vec<(i64, i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, changed_at, changes FROM inventory_changes
         WHERE host_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![host_id, before, limit], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    rows.collect::<rusqlite::Result<_>>()
}

/// 行程快照（原本在 `api/assets.rs`）
pub fn process_snapshot_at(
    conn: &Connection,
    host_id: i64,
    at_ms: i64,
) -> rusqlite::Result<Option<(i64, Vec<u8>)>> {
    conn.query_row(
        "SELECT ts, body FROM process_snapshots WHERE host_id = ?1 AND ts <= ?2
         ORDER BY ts DESC LIMIT 1",
        params![host_id, at_ms],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// 清冊中所有主機的 agent_id（原本在 `api/mod.rs`）
pub fn all_agent_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT agent_id FROM hosts")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
}

/// 把被拒的憑證寫入主機清冊（原本在 `api/maint.rs` 的 `persist_rejections`）
#[allow(clippy::type_complexity)]
pub fn store_cert_rejections(
    conn: &Connection,
    rows: &[(String, i64, String, Option<String>, Option<String>, i64)],
) -> rusqlite::Result<Vec<usize>> {
    let tx = conn.unchecked_transaction()?;
    let mut out = Vec::with_capacity(rows.len());
    {
        let mut stmt = tx.prepare(
            "UPDATE hosts SET cert_rejected_at = ?2,
                cert_rejection = json_object('reason', ?3, 'serial', ?4, 'peer', ?5,
                    'count', coalesce(json_extract(cert_rejection, '$.count'), 0) + ?6)
             WHERE agent_id = ?1 AND (cert_rejected_at IS NULL OR cert_rejected_at <= ?2)",
        )?;
        for (id, at, reason, serial, peer, delta) in rows {
            out.push(stmt.execute(params![id, at, reason, serial, peer, delta])?);
        }
    }
    tx.commit()?;
    Ok(out)
}

/// 主機的名稱（原本在 `api/privacy.rs`）
pub fn host_names(
    conn: &Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<(String, Option<String>)>> {
    conn.query_row(
        "SELECT hostname, display_name FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}
