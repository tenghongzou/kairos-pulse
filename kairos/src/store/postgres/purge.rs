//! 刪除一台主機的資料：[`crate::store::purge`] 中直接操作資料庫的函式的 PostgreSQL 實作（逐函式對照
//! `store/purge.rs`；說明、刪除的資料表與順序見該檔）。型別（[`PurgeJob`]、[`PurgeCounts`]、[`Stage`]）
//! 與常數沿用 `store/purge.rs`。

use serde_json::json;

use super::conn::{OptionalExtension, Params, PgConn, PgResult, params};
use crate::store::db::TxMode;
use crate::store::purge::{CODE_PURGED, CRL_KEEP_AFTER_EXPIRY_MS, PurgeCounts, PurgeJob, Stage};

/// 主機編號（清冊中沒有時為 `None`：例如只有註冊申請、尚未簽發）
pub fn host_id_of(conn: &PgConn, agent_id: &str) -> PgResult<Option<i64>> {
    conn.query_row(
        "SELECT host_id FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| r.get(0),
    )
    .optional()
}

fn count(conn: &PgConn, sql: &str, p: impl Params) -> PgResult<u64> {
    conn.query_row(sql, p, |r| r.get::<_, i64>(0))
        .map(|n| n.max(0) as u64)
}

// 憑證的三個查詢共用同一組參數（?1 agent_id、?2 CRL 保留時間、?3 現在）；參數明確轉型成 bigint，
// 沒用到的參數以 `?n::bigint = ?n::bigint` 帶過（PostgreSQL 要求每個參數都出現且能決定型別）

/// 已撤銷、撤銷已生效（或排定）、且還在 CRL 期間的憑證：不能刪
const CERT_KEPT: &str = "revoked_at IS NOT NULL AND not_after + ?2::bigint > ?3::bigint";
/// 尚未撤銷、尚未過期
const CERT_ACTIVE: &str = "revoked_at IS NULL AND not_after > ?3::bigint";
/// 這台主機所有的憑證
const CERT_ALL: &str = "SELECT COUNT(*) FROM pki_certificates
     WHERE agent_id = ?1 AND ?2::bigint = ?2::bigint AND ?3::bigint = ?3::bigint";

/// 試執行與影響預覽：各資料表會刪除的列數。`agent_id` 在任何資料表都沒有資料時全部為 0
pub fn plan(conn: &PgConn, agent_id: &str, now_ms: i64) -> PgResult<PurgeCounts> {
    let mut c = PurgeCounts::default();
    if let Some(h) = host_id_of(conn, agent_id)? {
        c.hosts = 1;
        c.reports = count(conn, "SELECT COUNT(*) FROM reports WHERE host_id = ?1", [h])?;
        c.process_snapshots = count(
            conn,
            "SELECT COUNT(*) FROM process_snapshots WHERE host_id = ?1",
            [h],
        )?;
        c.rollup_5m = count(
            conn,
            "SELECT COUNT(*) FROM rollup_5m WHERE host_id = ?1",
            [h],
        )?;
        c.rollup_1h = count(
            conn,
            "SELECT COUNT(*) FROM rollup_1h WHERE host_id = ?1",
            [h],
        )?;
        c.inventory = count(
            conn,
            "SELECT COUNT(*) FROM host_inventory WHERE host_id = ?1",
            [h],
        )?;
        c.inventory_changes = count(
            conn,
            "SELECT COUNT(*) FROM inventory_changes WHERE host_id = ?1",
            [h],
        )?;
        c.host_series = count(
            conn,
            "SELECT COUNT(*) FROM host_series WHERE host_id = ?1",
            [h],
        )?;
        c.boots = count(conn, "SELECT COUNT(*) FROM boots WHERE host_id = ?1", [h])?;
        c.import_keys = count(
            conn,
            "SELECT COUNT(*) FROM import_keys WHERE host_id = ?1",
            [h],
        )?;
        c.remote_management += count(
            conn,
            "SELECT COUNT(*) FROM rollout_hosts WHERE host_id = ?1",
            [h],
        )?;
    }
    c.remote_management += count(
        conn,
        "SELECT COUNT(*) FROM host_config WHERE agent_id = ?1",
        [agent_id],
    )?;
    c.alert_events = count(
        conn,
        "SELECT COUNT(*) FROM alert_events WHERE agent_id = ?1",
        [agent_id],
    )?;
    c.alert_timeline = count(
        conn,
        "SELECT COUNT(*) FROM alert_timeline WHERE event_id IN
         (SELECT event_id FROM alert_events WHERE agent_id = ?1)",
        [agent_id],
    )?;
    c.alert_deliveries = count(
        conn,
        "SELECT COUNT(*) FROM alert_deliveries WHERE event_id IN
         (SELECT event_id FROM alert_events WHERE agent_id = ?1)",
        [agent_id],
    )?;
    c.enroll_requests = count(
        conn,
        "SELECT COUNT(*) FROM enroll_requests WHERE agent_id = ?1",
        [agent_id],
    )?;
    let p = params![agent_id, CRL_KEEP_AFTER_EXPIRY_MS, now_ms];
    let all = count(conn, CERT_ALL, p.clone())?;
    c.certificates_active = count(
        conn,
        &format!(
            "SELECT COUNT(*) FROM pki_certificates
             WHERE agent_id = ?1 AND ?2::bigint = ?2::bigint AND {CERT_ACTIVE}"
        ),
        p.clone(),
    )?;
    let kept = count(
        conn,
        &format!("SELECT COUNT(*) FROM pki_certificates WHERE agent_id = ?1 AND {CERT_KEPT}"),
        p,
    )?;
    // 尚未撤銷的會先撤銷，撤銷後也要保留到過期
    c.certificates_kept = kept + c.certificates_active;
    c.certificates = all.saturating_sub(c.certificates_kept);
    Ok(c)
}

/// 做一步（一個交易）。`budget`：大量資料表每一步最多刪除的列數。回傳是否已完成
pub fn step(conn: &PgConn, job: &mut PurgeJob, budget: i64, now_ms: i64) -> PgResult<bool> {
    if !job.started {
        job.host_id = host_id_of(conn, &job.agent_id)?;
        job.started = true;
    }
    let tx = conn.begin_tx(TxMode::Immediate)?;
    let mut left = budget.max(1);
    while job.stage != Stage::Finish && job.stage != Stage::Done {
        let Some(h) = job.host_id else {
            job.stage = Stage::Finish;
            break;
        };
        let (sql, slot): (&str, &mut u64) = match job.stage {
            Stage::Reports => (
                "DELETE FROM reports WHERE id IN (SELECT id FROM reports WHERE host_id = ?1 LIMIT ?2)",
                &mut job.counts.reports,
            ),
            Stage::Processes => (
                "DELETE FROM process_snapshots WHERE (host_id, ts) IN
                 (SELECT host_id, ts FROM process_snapshots WHERE host_id = ?1 LIMIT ?2)",
                &mut job.counts.process_snapshots,
            ),
            Stage::Rollup5m => (
                "DELETE FROM rollup_5m WHERE (host_id, bucket, series_id) IN
                 (SELECT host_id, bucket, series_id FROM rollup_5m WHERE host_id = ?1 LIMIT ?2)",
                &mut job.counts.rollup_5m,
            ),
            Stage::Rollup1h => (
                "DELETE FROM rollup_1h WHERE (host_id, bucket, series_id) IN
                 (SELECT host_id, bucket, series_id FROM rollup_1h WHERE host_id = ?1 LIMIT ?2)",
                &mut job.counts.rollup_1h,
            ),
            _ => (
                "DELETE FROM inventory_changes WHERE id IN
                 (SELECT id FROM inventory_changes WHERE host_id = ?1 LIMIT ?2)",
                &mut job.counts.inventory_changes,
            ),
        };
        let n = tx.execute(sql, params![h, left])? as i64;
        *slot += n as u64;
        left -= n;
        if left <= 0 {
            // 額度用完：這個資料表可能還有，下一步從同一個資料表繼續
            tx.commit()?;
            return Ok(false);
        }
        job.stage = match job.stage {
            Stage::Reports => Stage::Processes,
            Stage::Processes => Stage::Rollup5m,
            Stage::Rollup5m => Stage::Rollup1h,
            Stage::Rollup1h => Stage::Changes,
            _ => Stage::Finish,
        };
    }
    if job.stage == Stage::Done {
        return Ok(true);
    }
    // 最後一步：其餘的資料表、清冊、稽核紀錄在同一個交易
    let a = job.agent_id.as_str();
    let c = &mut job.counts;
    if let Some(h) = job.host_id {
        c.inventory += tx.execute("DELETE FROM host_inventory WHERE host_id = ?1", [h])? as u64;
        c.host_series += tx.execute("DELETE FROM host_series WHERE host_id = ?1", [h])? as u64;
        c.boots += tx.execute("DELETE FROM boots WHERE host_id = ?1", [h])? as u64;
        c.import_keys += tx.execute("DELETE FROM import_keys WHERE host_id = ?1", [h])? as u64;
        c.remote_management +=
            tx.execute("DELETE FROM rollout_hosts WHERE host_id = ?1", [h])? as u64;
    }
    c.remote_management += tx.execute("DELETE FROM host_config WHERE agent_id = ?1", [a])? as u64;
    let events = "SELECT event_id FROM alert_events WHERE agent_id = ?1";
    c.alert_timeline += tx.execute(
        &format!("DELETE FROM alert_timeline WHERE event_id IN ({events})"),
        [a],
    )? as u64;
    c.alert_deliveries += tx.execute(
        &format!("DELETE FROM alert_deliveries WHERE event_id IN ({events})"),
        [a],
    )? as u64;
    tx.execute(
        &format!("DELETE FROM alert_actions WHERE event_id IN ({events})"),
        [a],
    )?;
    c.alert_events += tx.execute("DELETE FROM alert_events WHERE agent_id = ?1", [a])? as u64;
    c.enroll_requests += tx.execute("DELETE FROM enroll_requests WHERE agent_id = ?1", [a])? as u64;
    tx.execute("DELETE FROM enroll_replay WHERE agent_id = ?1", [a])?;
    let p = params![a, CRL_KEEP_AFTER_EXPIRY_MS, now_ms];
    // 尚未撤銷的有效憑證也不刪（呼叫端應先撤銷：刪掉紀錄之後就無法再撤銷它）
    c.certificates += tx.execute(
        &format!(
            "DELETE FROM pki_certificates WHERE agent_id = ?1 AND NOT ({CERT_KEPT}) AND NOT ({CERT_ACTIVE})"
        ),
        p.clone(),
    )? as u64;
    c.certificates_kept = count(&tx, CERT_ALL, p.clone())?;
    c.certificates_active = count(
        &tx,
        &format!(
            "SELECT COUNT(*) FROM pki_certificates
             WHERE agent_id = ?1 AND ?2::bigint = ?2::bigint AND {CERT_ACTIVE}"
        ),
        p,
    )?;
    c.hosts += tx.execute("DELETE FROM hosts WHERE agent_id = ?1", [a])? as u64;
    let mut details = json!({
        "host_id": job.host_id,
        "deleted": c.json(),
        "revoked_serials": job.revoked,
        "audit_log": "kept",
    });
    if let Some(r) = &job.reason {
        details["reason"] = json!(r);
    }
    // 稽核鏈最後寫（鎖的順序：業務列 → 稽核鏈，`kairos/HA.md` 第 5.4 節）
    super::audit::append(
        &tx,
        now_ms,
        &crate::audit::Audit {
            actor: &job.actor,
            actor_ip: job.actor_ip.as_deref(),
            category: "host",
            action: "host.purge",
            object: Some(a),
            result: "ok",
            code: Some(CODE_PURGED),
            details: Some(details),
        },
    )?;
    tx.commit()?;
    job.stage = Stage::Done;
    Ok(true)
}

/// 一次做完（`kairos hosts purge`：Kairos 停止、已取得獨占鎖）
pub fn run_to_end(conn: &PgConn, job: &mut PurgeJob, now_ms: i64) -> PgResult<()> {
    while !step(conn, job, 20_000, now_ms)? {}
    Ok(())
}
