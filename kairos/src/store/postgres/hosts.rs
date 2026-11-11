//! [`crate::store::hosts`] 的 PostgreSQL 實作（[`HostStore`]）。逐函式對照 `store/sqlite/hosts.rs`，另含
//! SQLite 版放在 `store/sqlite.rs` 的 [`list_hosts`]、[`export_reports`]、[`backup_to`]。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use std::collections::{BTreeMap, HashMap};

use chrono::DateTime;

#[allow(unused_imports)]
use super::conn::{OptionalExtension, PgConn, PgError, PgResult, Row, Value, params, pg_domain};
use crate::storage::{RecordBody, ReportRecord};
use crate::store::codec::{self, AgentTs, FORMAT_LEGACY};
use crate::store::db::TxMode;
#[allow(unused_imports)]
use crate::store::hosts::*;
use crate::store::sqlite::HostRow;

pg_domain! { HostStore {
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
    fn purge_plan(&self, agent_id: &str, now_ms: i64) -> crate::store::purge::PurgeCounts => super::purge::plan;
    fn list_hosts(&self) -> Vec<crate::store::sqlite::HostRow> => list_hosts;
    fn export_reports(&self, since_ms: i64, agent_id: Option<&str>, out: &mut dyn FnMut(&crate::storage::ReportRecord) -> Result<(), std::io::Error>) -> u64 => export_reports;
    fn backup_to(&self, dest: &std::path::Path) -> () => backup_to;
    fn purge_run_to_end(&self, job: &mut crate::store::purge::PurgeJob, now_ms: i64) -> () => super::purge::run_to_end;
    fn host_meta_rows(&self) -> Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> => host_meta_rows;
} }

/// 資料庫的「頁大小」與「使用中的頁數」：PostgreSQL 沒有 SQLite 的頁統計，改以目前 schema 中所有資料表
/// （含索引與 TOAST）的 `pg_total_relation_size` 加總換算——頁大小是伺服器的 `block_size`（通常 8192），
/// 頁數是加總除以頁大小（無條件進位），兩者相乘就是使用的位元組數（`kairos/HA.md` 第 7.5 節）
pub fn db_page_stats(conn: &PgConn) -> PgResult<(i64, i64)> {
    let (page_size, total): (i64, i64) = conn.query_row(
        "SELECT current_setting('block_size')::bigint,
                (SELECT COALESCE(SUM(pg_total_relation_size(c.oid)), 0)::bigint
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = current_schema() AND c.relkind = 'r')",
        (),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let page_size = page_size.max(1);
    Ok((page_size, (total + page_size - 1) / page_size))
}

/// 主機的 host_id（原本在 `api/metrics.rs`）
pub fn host_id_of(conn: &PgConn, agent_id: &str) -> PgResult<Option<i64>> {
    conn.query_row(
        "SELECT host_id FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()
}

/// 指標趨勢用的主機資訊（原本在 `api/metrics.rs` 的 `run_query`）
pub fn trend_host(conn: &PgConn, agent_id: &str) -> PgResult<Option<(i64, i64, Option<i64>)>> {
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
    conn: &PgConn,
    host_id: i64,
    name: &str,
) -> PgResult<Vec<(i64, String, String, i64)>> {
    let collect = |sql: &str, p: Vec<Value>| -> PgResult<Vec<_>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    };
    let list = collect(
        "SELECT s.series_id, s.unit, s.labels, s.derived FROM series s
         JOIN host_series h ON h.series_id = s.series_id WHERE h.host_id = ?1 AND s.name = ?2 ORDER BY s.series_id",
        params![host_id, name],
    )?;
    if !list.is_empty() {
        return Ok(list);
    }
    collect(
        "SELECT series_id, unit, labels, derived FROM series WHERE name = ?1",
        params![name],
    )
}

/// 標準序列的 series_id（原本在 `api/metrics.rs`）
pub fn standard_series_id(conn: &PgConn, name: &str, unit: &str) -> PgResult<Option<i64>> {
    conn.query_row(
        "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = '' AND derived = 0",
        params![name, unit],
        |r| r.get(0),
    )
    .optional()
}

/// 開機時間序列的 series_id（原本在 `api/metrics.rs`）
pub fn uptime_series_id(conn: &PgConn, name: &str) -> PgResult<Option<i64>> {
    conn.query_row(
        "SELECT series_id FROM series WHERE name = ?1 AND unit = 's' AND labels = '' AND derived = 0",
        [name],
        |r| r.get(0),
    )
    .optional()
}

/// 序列的名稱、單位、label 與推算種類（原本在 `api/metrics.rs` 的 `available`）
pub fn series_info(
    conn: &PgConn,
    series_id: i64,
) -> PgResult<Option<(String, String, String, i64)>> {
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
pub fn report_ts_before(conn: &PgConn, host_id: i64, before_ms: i64) -> PgResult<Option<i64>> {
    conn.query_row(
        "SELECT ts FROM reports WHERE host_id = ?1 AND ts < ?2 ORDER BY ts DESC LIMIT 1",
        params![host_id, before_ms],
        |r| r.get(0),
    )
    .optional()
}

/// 最新的一筆 report（原本在 `api/metrics.rs` 的 `available`）
pub fn latest_report(conn: &PgConn, host_id: i64) -> PgResult<Option<(i64, i64, Vec<u8>)>> {
    conn.query_row(
        "SELECT ts, format, body FROM reports WHERE host_id = ?1 ORDER BY ts DESC LIMIT 1",
        [host_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

/// 逐筆讀取原始 report（原本在 `api/metrics.rs` 的 `read_reports`）。串流讀取：進行中這條連線不能送出
/// 其他敘述（`f` 不可以再用同一條連線查詢）
pub fn for_each_report(
    conn: &PgConn,
    host_id: i64,
    from_ms: i64,
    to_ms: i64,
    f: &mut dyn FnMut(i64, i64, &[u8]),
) -> PgResult<()> {
    conn.query_each(
        "SELECT ts, format, body FROM reports WHERE host_id = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts",
        params![host_id, from_ms, to_ms],
        |r| {
            let t: i64 = r.get(0)?;
            let format: i64 = r.get(1)?;
            if let Value::Blob(body) = r.get::<_, Value>(2)? {
                f(t, format, &body);
            }
            Ok(true)
        },
    )
}

/// 逐列讀取彙總（原本在 `api/metrics.rs` 的 `run_query`）。`table` 只能是 `rollup_5m` 或 `rollup_1h`。
/// 串流讀取：進行中這條連線不能送出其他敘述
pub fn for_each_rollup(
    conn: &PgConn,
    table: &str,
    host_id: i64,
    from_bucket: i64,
    to_bucket: i64,
    wanted: &dyn Fn(i64) -> bool,
    f: &mut dyn FnMut(i64, i64, crate::store::rollup::Agg),
) -> PgResult<()> {
    debug_assert!(table == "rollup_5m" || table == "rollup_1h");
    conn.query_each(
        &format!(
            "SELECT bucket, series_id, vmin, vmax, vsum, cnt FROM {table}
             WHERE host_id = ?1 AND bucket >= ?2 AND bucket < ?3 ORDER BY bucket, series_id"
        ),
        params![host_id, from_bucket, to_bucket],
        |r| {
            let id: i64 = r.get(1)?;
            if !wanted(id) {
                return Ok(true);
            }
            let agg = crate::store::rollup::Agg {
                min: r.get(2)?,
                max: r.get(3)?,
                sum: r.get(4)?,
                count: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
            };
            let bucket: i64 = r.get(0)?;
            f(bucket, id, agg);
            Ok(true)
        },
    )
}

/// 最近的 1 小時彙總中的序列（原本在 `api/metrics.rs` 的 `available`）
pub fn recent_rollup_series(conn: &PgConn, host_id: i64, since_bucket: i64) -> PgResult<Vec<i64>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT series_id FROM rollup_1h WHERE host_id = ?1 AND bucket >= ?2 ORDER BY series_id")?;
    let rows = stmt.query_map(params![host_id, since_bucket], |r| r.get(0))?;
    rows.collect::<PgResult<_>>()
}

/// 主機的群組（原本在 `api/hosts.rs`）
pub fn host_group_of(conn: &PgConn, agent_id: &str) -> PgResult<Option<Option<String>>> {
    conn.query_row(
        "SELECT host_group FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
}

/// 主機編號、`os`、開機時間（原本在 `api/assets.rs`）
pub fn host_basics(
    conn: &PgConn,
    agent_id: &str,
) -> PgResult<Option<(i64, Option<String>, Option<i64>)>> {
    conn.query_row(
        "SELECT host_id, os, boot_time FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

/// 主機目前的資產（原本在 `api/assets.rs`）
pub fn host_inventory(
    conn: &PgConn,
    host_id: i64,
) -> PgResult<Option<(String, String, i64, i64, i64)>> {
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
    conn: &PgConn,
    host_id: i64,
    before: i64,
    limit: i64,
) -> PgResult<Vec<(i64, i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, changed_at, changes FROM inventory_changes
         WHERE host_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![host_id, before, limit], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    rows.collect::<PgResult<_>>()
}

/// 行程快照（原本在 `api/assets.rs`）
pub fn process_snapshot_at(
    conn: &PgConn,
    host_id: i64,
    at_ms: i64,
) -> PgResult<Option<(i64, Vec<u8>)>> {
    conn.query_row(
        "SELECT ts, body FROM process_snapshots WHERE host_id = ?1 AND ts <= ?2
         ORDER BY ts DESC LIMIT 1",
        params![host_id, at_ms],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// 清冊中所有主機的 agent_id（原本在 `api/mod.rs`）
pub fn all_agent_ids(conn: &PgConn) -> PgResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT agent_id FROM hosts ORDER BY agent_id")?;
    let rows = stmt.query_map((), |r| r.get::<_, String>(0))?;
    rows.collect::<PgResult<Vec<_>>>()
}

/// 把被拒的憑證寫入主機清冊（原本在 `api/maint.rs` 的 `persist_rejections`）。次數的累加在 UPDATE 內
/// 以該列當下的值計算（原子）
#[allow(clippy::type_complexity)]
pub fn store_cert_rejections(
    conn: &PgConn,
    rows: &[(String, i64, String, Option<String>, Option<String>, i64)],
) -> PgResult<Vec<usize>> {
    let tx = conn.begin_tx(TxMode::Deferred)?;
    let mut out = Vec::with_capacity(rows.len());
    {
        let mut stmt = tx.prepare(
            "UPDATE hosts SET cert_rejected_at = ?2,
                cert_rejection = jsonb_build_object('reason', ?3::text, 'serial', ?4::text,
                    'peer', ?5::text,
                    'count', COALESCE((cert_rejection::jsonb ->> 'count')::bigint, 0) + ?6::bigint)::text
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
pub fn host_names(conn: &PgConn, agent_id: &str) -> PgResult<Option<(String, Option<String>)>> {
    conn.query_row(
        "SELECT hostname, display_name FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// 主機屬性（原本在 `export/otlp.rs` 的 `load_host_meta`）
#[allow(clippy::type_complexity)]
pub fn host_meta_rows(
    conn: &PgConn,
) -> PgResult<
    Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )>,
> {
    let mut stmt = conn.prepare(
        "SELECT h.agent_id, h.display_name, h.host_group, h.tags, h.os, i.os_family
         FROM hosts h LEFT JOIN host_inventory i ON i.host_id = h.host_id ORDER BY h.host_id",
    )?;
    let rows = stmt.query_map((), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    rows.collect()
}

// ---- SQLite 版放在 `store/sqlite.rs` 的唯讀查詢與管理操作 ----

/// 清冊中所有主機（依 `last_seen` 由舊到新）
pub fn list_hosts(conn: &PgConn) -> PgResult<Vec<HostRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, hostname, display_name, agent_version, proto, host_fingerprint, peer,
                identity_verified, online, first_seen, last_seen, offline_after_ms
         FROM hosts ORDER BY last_seen, agent_id",
    )?;
    let time = |ms: i64| DateTime::from_timestamp_millis(ms).unwrap_or_default();
    let rows = stmt.query_map((), |r| {
        Ok(HostRow {
            agent_id: r.get(0)?,
            hostname: r.get(1)?,
            display_name: r.get(2)?,
            agent_version: r.get(3)?,
            proto: r.get(4)?,
            host_fingerprint: r.get(5)?,
            peer: r.get(6)?,
            identity_verified: r.get(7)?,
            online: r.get(8)?,
            first_seen: time(r.get(9)?),
            last_seen: time(r.get(10)?),
            offline_after_ms: r.get(11)?,
        })
    })?;
    rows.collect()
}

/// 匯出原始 report（`kairos reports export`）：取樣時間（`reports.ts`）不早於 `since_ms` 的 report，
/// 依寫入順序轉回 JSONL 的格式（[`ReportRecord`]，`recv_at` 為收到的時間 = `ts + recv_lag`）逐筆
/// 交給 `out`。資料庫不逐筆保存 `peer` 與 `hostname`，以主機清冊中最後的值代替。回傳匯出的筆數。
///
/// client 原本的 `ts` 在 PostgreSQL 拆成兩欄：`agent_ts_ms`（可無損還原的 RFC 3339，以毫秒保存）與
/// `agent_ts_text`（其餘原樣保存的字串），兩者最多一個非 NULL。report 以串流讀取（不全部放進記憶體）
pub fn export_reports(
    conn: &PgConn,
    since_ms: i64,
    agent_id: Option<&str>,
    out: &mut dyn FnMut(&ReportRecord) -> Result<(), std::io::Error>,
) -> PgResult<u64> {
    let mut series: HashMap<i64, (String, String, BTreeMap<String, String>)> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT series_id, name, unit, labels FROM series")?;
        let rows = stmt.query_map((), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, name, unit, labels) = row?;
            let labels = if labels.is_empty() {
                Default::default()
            } else {
                serde_json::from_str(&labels).unwrap_or_default()
            };
            series.insert(id, (name, unit, labels));
        }
    }
    let mut n = 0;
    conn.query_each(
        "SELECT r.ts, r.seq, r.agent_ts_ms, r.agent_ts_text, r.format, r.body, b.boot_id,
                r.report_seq, h.agent_id, h.hostname, h.peer, r.recv_lag
         FROM reports r JOIN hosts h ON h.host_id = r.host_id
              LEFT JOIN boots b ON b.boot_ref = r.boot_ref
         WHERE r.ts >= ?1 AND (?2::text IS NULL OR h.agent_id = ?2::text)
         ORDER BY r.id",
        params![since_ms, agent_id],
        |r| {
            let ts: i64 = r.get(0)?;
            let agent_ts = match (r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?) {
                (Some(ms), _) => AgentTs::Millis(ms).to_text(),
                (None, Some(t)) => t,
                (None, None) => String::new(),
            };
            let body: Vec<u8> = r.get(5)?;
            let corrupt = || PgError::other(format!("report 內容損毀（ts = {ts}）"));
            let body = match r.get::<_, i64>(4)? {
                FORMAT_LEGACY => {
                    let (cpu_usage, total_memory, used_memory) =
                        codec::decode_legacy(&body).ok_or_else(corrupt)?;
                    RecordBody::Legacy {
                        cpu_usage,
                        total_memory,
                        used_memory,
                    }
                }
                _ => {
                    let mut metrics = Vec::new();
                    for (id, value) in codec::decode_metrics(&body).ok_or_else(corrupt)? {
                        let (name, unit, labels) = series.get(&id).ok_or_else(corrupt)?;
                        metrics.push(crate::protocol::Metric {
                            name: name.clone(),
                            value,
                            unit: unit.clone(),
                            labels: labels.clone(),
                        });
                    }
                    RecordBody::Metrics { metrics }
                }
            };
            let dedup = match (r.get::<_, Option<String>>(6)?, r.get::<_, Option<i64>>(7)?) {
                (Some(boot_id), Some(seq)) => Some(crate::storage::DedupKey {
                    boot_id,
                    report_seq: seq as u64,
                }),
                _ => None,
            };
            let recv_lag: Option<i64> = r.get(11)?;
            let recv_at = ts.saturating_add(recv_lag.unwrap_or(0));
            let record = ReportRecord::new(
                DateTime::from_timestamp_millis(recv_at).unwrap_or_default(),
                r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                r.get(8)?,
                r.get(9)?,
                r.get::<_, i64>(1)? as u64,
                agent_ts,
                body,
            )
            .with_dedup(dedup);
            // 輸出端的錯誤（磁碟已滿、管線中斷）：訊息原樣帶回，停止匯出
            out(&record).map_err(PgError::Io)?;
            n += 1;
            Ok(true)
        },
    )?;
    Ok(n)
}

/// `kairos db backup` 在 PostgreSQL 後端不適用（SQLite 版是 `VACUUM INTO`）：一律拒絕並提示改用資料庫的
/// 備份工具（`kairos/HA.md` 第 16.3 節）。不建立 `dest`
pub fn backup_to(_conn: &PgConn, dest: &std::path::Path) -> PgResult<()> {
    Err(PgError::other(format!(
        "PostgreSQL 後端不支援 kairos db backup（未建立 {}）：請改用 pg_dump、pgBackRest 等資料庫的備份工具，\
         或以 kairos db migrate --to sqlite:<檔案> 產生可攜的 SQLite 快照（kairos/HA.md 第 16.3 節）",
        dest.display()
    )))
}
