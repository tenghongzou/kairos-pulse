//! [`crate::cluster::store`] 的 SQLite 實作（[`ClusterStore`]）與 schema v10 的叢集資料表。
//!
//! 叢集模式只支援 PostgreSQL；SQLite 也建立同樣的資料表（`kairos/HA.md` 第 5.5 節：兩個後端的資料表一致，
//! `kairos db migrate` 才能一對一複製），單機時保持空白。

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::conn::{AsConn, forward};
use crate::cluster::store::{ClusterStore, LeaseRow, NodeRegistration, NodeRow, sql};

/// schema v10（0.10.0〔草稿〕）：叢集協調（`kairos/HA.md` 第 5.5 節）。**獨立的一步**：只新增資料表與
/// 三個可為 NULL 的欄位，不修改既有的資料與唯一鍵；與其他線新增的 schema 版本沒有相依，整合時可以直接
/// 重新編號（[`super::MIGRATIONS`] 的版本號與 `store/postgres/schema.rs` 的 `UPGRADES` 一起改）。
///
/// 階段 A 實際使用的是 `cluster_nodes`、`cluster_leases`；其餘資料表與欄位（`cluster_revs`、
/// `host_presence`、`cluster_events`、`alert_event_seq`、`login_challenges`、`enroll_nonces`、
/// `auth_failures`、`enroll_requests.issuing_node`／`issuing_at`、`alert_deliveries.claim_epoch`）
/// 依規格先建立，階段 B 才寫入。與 HA.md 第 5.5 節的差異：沒有 `enroll_requests_issued_csr`（既有的
/// `enroll_requests_csr` 已是 `(agent_id, csr_sha256)` 的唯一索引）；`host_presence` 的序列
/// （`conn_epoch_seq`、`presence_rev_seq`）留到階段 B。
pub(crate) const SCHEMA_V10: &str = "
CREATE TABLE cluster_nodes (
    node_id        TEXT PRIMARY KEY,
    hostname       TEXT NOT NULL,
    version        TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    role           TEXT NOT NULL,
    config_hash    TEXT NOT NULL,
    keys_hash      TEXT NOT NULL,
    advertise      TEXT,
    started_at     INTEGER NOT NULL,
    heartbeat_at   INTEGER NOT NULL,
    info           TEXT
) WITHOUT ROWID;

CREATE TABLE cluster_leases (
    name        TEXT PRIMARY KEY,
    holder      TEXT NOT NULL,
    epoch       INTEGER NOT NULL,
    acquired_at INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE cluster_revs (
    name TEXT PRIMARY KEY,
    rev  INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE host_presence (
    host_id        INTEGER PRIMARY KEY,
    conn_node      TEXT,
    conn_epoch     INTEGER NOT NULL,
    online         INTEGER NOT NULL,
    connected_at   INTEGER,
    last_seen      INTEGER NOT NULL,
    offline_reason TEXT,
    peer           TEXT,
    rev            INTEGER NOT NULL
);
CREATE INDEX host_presence_rev ON host_presence (rev);
CREATE INDEX host_presence_node ON host_presence (conn_node) WHERE online = 1;

CREATE TABLE cluster_events (
    id      INTEGER PRIMARY KEY,
    at      INTEGER NOT NULL,
    node_id TEXT NOT NULL,
    kind    TEXT NOT NULL,
    target  TEXT,
    payload TEXT NOT NULL
);

CREATE TABLE alert_event_seq (
    day TEXT PRIMARY KEY,
    seq INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE login_challenges (
    challenge_hash BLOB PRIMARY KEY,
    user_id        INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    attempts       INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE enroll_nonces (
    nonce_hash BLOB PRIMARY KEY,
    request_id TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE auth_failures (
    source     TEXT PRIMARY KEY,
    tokens     REAL NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

ALTER TABLE enroll_requests ADD COLUMN issuing_node TEXT;
ALTER TABLE enroll_requests ADD COLUMN issuing_at INTEGER;
ALTER TABLE alert_deliveries ADD COLUMN claim_epoch INTEGER;
";

impl<C: AsConn> ClusterStore for C {
    forward! {
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
    }
}

fn node_row(r: &Row<'_>) -> rusqlite::Result<NodeRow> {
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

fn lease_row(r: &Row<'_>) -> rusqlite::Result<LeaseRow> {
    Ok(LeaseRow {
        holder: r.get(0)?,
        epoch: r.get(1)?,
        acquired_at: r.get(2)?,
        expires_at: r.get(3)?,
        now: r.get(4)?,
    })
}

pub fn node(
    conn: &Connection,
    node_id: &str,
    now: Option<i64>,
) -> rusqlite::Result<Option<NodeRow>> {
    conn.query_row(&sql::nodes(false), params![node_id, now], node_row)
        .optional()
}

pub fn nodes(conn: &Connection, now: Option<i64>) -> rusqlite::Result<Vec<NodeRow>> {
    let mut stmt = conn.prepare(&sql::nodes(false))?;
    let rows = stmt.query_map(params![None::<String>, now], node_row)?;
    rows.collect()
}

pub fn register(
    conn: &Connection,
    reg: &NodeRegistration<'_>,
    now: Option<i64>,
) -> rusqlite::Result<i64> {
    conn.query_row(
        &sql::register(false),
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
    conn: &Connection,
    node_id: &str,
    started_at: i64,
    role: &str,
    keys_hash: &str,
    info: Option<&str>,
    now: Option<i64>,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        &sql::heartbeat(false),
        params![node_id, started_at, role, keys_hash, info, now],
        |r| r.get(0),
    )
    .optional()
}

pub fn set_role(conn: &Connection, node_id: &str, role: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE cluster_nodes SET role = ?2 WHERE node_id = ?1",
        params![node_id, role],
    )? > 0)
}

pub fn forget(conn: &Connection, node_id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM cluster_nodes WHERE node_id = ?1", [node_id])? > 0)
}

pub fn mark_lost(
    conn: &Connection,
    lost_after_ms: i64,
    except: &str,
    now: Option<i64>,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&sql::mark_lost(false))?;
    let rows = stmt.query_map(params![lost_after_ms, except, now], |r| r.get(0))?;
    rows.collect()
}

pub fn lease(
    conn: &Connection,
    name: &str,
    now: Option<i64>,
) -> rusqlite::Result<Option<LeaseRow>> {
    conn.query_row(&sql::lease(false), params![name, now], lease_row)
        .optional()
}

pub fn acquire(
    conn: &Connection,
    name: &str,
    node: &str,
    ttl_ms: i64,
    now: Option<i64>,
) -> rusqlite::Result<Option<LeaseRow>> {
    conn.query_row(
        &sql::acquire(false),
        params![name, node, ttl_ms, now],
        lease_row,
    )
    .optional()
}

pub fn renew(
    conn: &Connection,
    name: &str,
    node: &str,
    epoch: i64,
    ttl_ms: i64,
    now: Option<i64>,
) -> rusqlite::Result<Option<LeaseRow>> {
    conn.query_row(
        &sql::renew(false),
        params![name, node, epoch, ttl_ms, now],
        lease_row,
    )
    .optional()
}

pub fn release(
    conn: &Connection,
    name: &str,
    node: &str,
    epoch: i64,
    now: Option<i64>,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(&sql::release(false), params![name, node, epoch, now])? > 0)
}

pub fn vacate(
    conn: &Connection,
    name: &str,
    now: Option<i64>,
) -> rusqlite::Result<Option<LeaseRow>> {
    conn.query_row(&sql::vacate(false), params![name, now], lease_row)
        .optional()
}
