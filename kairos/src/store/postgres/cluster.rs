//! [`crate::cluster::store`] 的 PostgreSQL 實作（[`ClusterStore`]）。SQL 與 `store/sqlite/cluster.rs` 相同
//! （[`sql`]），時間取自 `clock_timestamp()`。

use super::conn::{OptionalExtension, PgConn, PgResult, Row, params, pg_domain};
use crate::cluster::store::{ClusterStore, LeaseRow, NodeRegistration, NodeRow, sql};

pg_domain! { ClusterStore {
    fn cluster_node(&self, node_id: &str, now: Option<i64>) -> Option<NodeRow> => node;
    fn cluster_nodes(&self, now: Option<i64>) -> Vec<NodeRow> => nodes;
    fn cluster_node_register(&self, reg: &NodeRegistration<'_>, now: Option<i64>) -> i64 => register;
    fn cluster_node_heartbeat(&self, node_id: &str, started_at: i64, role: &str, keys_hash: &str, info: Option<&str>, now: Option<i64>) -> Option<String> => heartbeat;
    fn cluster_node_set_role(&self, node_id: &str, role: &str) -> bool => set_role;
    fn cluster_node_forget(&self, node_id: &str) -> bool => forget;
    fn cluster_mark_lost(&self, lost_after_ms: i64, except: &str, now: Option<i64>) -> Vec<String> => mark_lost;
    fn cluster_lease(&self, name: &str, now: Option<i64>) -> Option<LeaseRow> => lease;
    fn cluster_lease_acquire(&self, name: &str, node: &str, ttl_ms: i64, now: Option<i64>) -> Option<LeaseRow> => acquire;
    fn cluster_lease_renew(&self, name: &str, node: &str, epoch: i64, ttl_ms: i64, now: Option<i64>) -> Option<LeaseRow> => renew;
    fn cluster_lease_release(&self, name: &str, node: &str, epoch: i64, now: Option<i64>) -> bool => release;
    fn cluster_lease_vacate(&self, name: &str, now: Option<i64>) -> Option<LeaseRow> => vacate;
} }

fn node_row(r: &Row) -> PgResult<NodeRow> {
    Ok(NodeRow {
        node_id: r.get(0)?,
        hostname: r.get(1)?,
        version: r.get(2)?,
        schema_version: r.get(3)?,
        role: r.get(4)?,
        config_hash: r.get(5)?,
        keys_hash: r.get(6)?,
        advertise: r.get(7)?,
        started_at: r.get(8)?,
        heartbeat_at: r.get(9)?,
        info: r.get(10)?,
        heartbeat_age_ms: r.get(11)?,
    })
}

fn lease_row(r: &Row) -> PgResult<LeaseRow> {
    Ok(LeaseRow {
        holder: r.get(0)?,
        epoch: r.get(1)?,
        acquired_at: r.get(2)?,
        expires_at: r.get(3)?,
        now: r.get(4)?,
    })
}

pub fn node(conn: &PgConn, node_id: &str, now: Option<i64>) -> PgResult<Option<NodeRow>> {
    conn.query_row(&sql::nodes(true), params![node_id, now], node_row)
        .optional()
}

pub fn nodes(conn: &PgConn, now: Option<i64>) -> PgResult<Vec<NodeRow>> {
    let rows = conn.query_rows(&sql::nodes(true), params![None::<String>, now])?;
    rows.iter().map(node_row).collect()
}

pub fn register(conn: &PgConn, reg: &NodeRegistration<'_>, now: Option<i64>) -> PgResult<i64> {
    conn.query_row(
        &sql::register(true),
        params![
            reg.node_id,
            reg.hostname,
            reg.version,
            reg.schema_version,
            reg.config_hash,
            reg.keys_hash,
            reg.advertise,
            reg.info,
            now
        ],
        |r| r.get(0),
    )
}

pub fn heartbeat(
    conn: &PgConn,
    node_id: &str,
    started_at: i64,
    role: &str,
    keys_hash: &str,
    info: Option<&str>,
    now: Option<i64>,
) -> PgResult<Option<String>> {
    conn.query_row(
        &sql::heartbeat(true),
        params![node_id, started_at, role, keys_hash, info, now],
        |r| r.get(0),
    )
    .optional()
}

pub fn set_role(conn: &PgConn, node_id: &str, role: &str) -> PgResult<bool> {
    Ok(conn.execute(
        "UPDATE cluster_nodes SET role = ?2 WHERE node_id = ?1",
        params![node_id, role],
    )? > 0)
}

pub fn forget(conn: &PgConn, node_id: &str) -> PgResult<bool> {
    Ok(conn.execute("DELETE FROM cluster_nodes WHERE node_id = ?1", [node_id])? > 0)
}

pub fn mark_lost(
    conn: &PgConn,
    lost_after_ms: i64,
    except: &str,
    now: Option<i64>,
) -> PgResult<Vec<String>> {
    let rows = conn.query_rows(&sql::mark_lost(true), params![lost_after_ms, except, now])?;
    rows.iter().map(|r| r.get(0)).collect()
}

pub fn lease(conn: &PgConn, name: &str, now: Option<i64>) -> PgResult<Option<LeaseRow>> {
    conn.query_row(&sql::lease(true), params![name, now], lease_row)
        .optional()
}

pub fn acquire(
    conn: &PgConn,
    name: &str,
    node: &str,
    ttl_ms: i64,
    now: Option<i64>,
) -> PgResult<Option<LeaseRow>> {
    conn.query_row(
        &sql::acquire(true),
        params![name, node, ttl_ms, now],
        lease_row,
    )
    .optional()
}

pub fn renew(
    conn: &PgConn,
    name: &str,
    node: &str,
    epoch: i64,
    ttl_ms: i64,
    now: Option<i64>,
) -> PgResult<Option<LeaseRow>> {
    conn.query_row(
        &sql::renew(true),
        params![name, node, epoch, ttl_ms, now],
        lease_row,
    )
    .optional()
}

pub fn release(
    conn: &PgConn,
    name: &str,
    node: &str,
    epoch: i64,
    now: Option<i64>,
) -> PgResult<bool> {
    Ok(conn.execute(&sql::release(true), params![name, node, epoch, now])? > 0)
}

pub fn vacate(conn: &PgConn, name: &str, now: Option<i64>) -> PgResult<Option<LeaseRow>> {
    conn.query_row(&sql::vacate(true), params![name, now], lease_row)
        .optional()
}
