//! [`crate::store::api`] 的 PostgreSQL 實作（[`ApiQueries`]）。逐函式對照 `store/sqlite/api.rs`。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use std::collections::HashMap;

use chrono::DateTime;
use std::sync::Arc;

#[allow(unused_imports)]
use super::conn::{OptionalExtension, PgConn, PgResult, Row, params, pg_domain};
use crate::api::hosts::{DbHost, InventorySummary, StoredRejection};
#[allow(unused_imports)]
use crate::store::api::*;
use crate::store::sqlite::HOST_SOURCE_IMPORT;
use crate::tls::{CertRejection, RejectionReason};

pg_domain! { ApiQueries {
    fn cert_hosts(&self, serial: Option<&str>) -> std::collections::HashMap<String, (Option<String>, Option<String>)> => cert_hosts;
    fn load_hosts(&self) -> Vec<DbHost> => load_hosts;
    fn load_host(&self, agent_id: &str) -> Option<DbHost> => load_host;
    fn max_host_id(&self) -> i64 => max_host_id;
    fn latest_cert_of(&self, agent_id: &str) -> Option<(i64, Option<i64>)> => latest_cert_of;
} }

/// 憑證所屬主機的名稱與群組（原本在 `api/enroll.rs`）
pub fn cert_hosts(
    conn: &PgConn,
    serial: Option<&str>,
) -> PgResult<HashMap<String, (Option<String>, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT c.serial, coalesce(h.hostname, r.hostname), h.host_group
         FROM pki_certificates c
         LEFT JOIN hosts h ON h.agent_id = c.agent_id
         LEFT JOIN enroll_requests r ON r.request_id = c.request_id
         WHERE ?1::text IS NULL OR c.serial = ?1::text",
    )?;
    let rows = stmt.query_map([serial], |r| {
        Ok((
            r.get::<_, String>(0)?,
            (
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ),
        ))
    })?;
    rows.collect()
}

/// 清冊查詢另取的欄位：開機時間與資產摘要（以子查詢的別名避免與 `hosts` 的欄位同名）
const HOST_EXTRA_COLUMNS: &str = "boot_time, i_os_family, i_cpu_model, i_cores, i_memory, \
     i_vendor, i_product, i_serial, series_limited_at, install_method, install_upgrade, \
     install_config, hc_desired_rev, hc_applied_rev, hc_state, hc_overridden, \
     install_external_url";
/// PostgreSQL 的子查詢必須有別名（`inv`、`hc`）
const HOST_EXTRA_JOIN: &str = "LEFT JOIN (SELECT host_id AS i_host, os_family AS i_os_family, \
     cpu_model AS i_cpu_model, cpu_logical_cores AS i_cores, memory_total AS i_memory, \
     vendor AS i_vendor, product AS i_product, serial AS i_serial FROM host_inventory) inv \
     ON inv.i_host = hosts.host_id \
     LEFT JOIN (SELECT agent_id AS hc_agent, desired_rev AS hc_desired_rev, applied_rev AS hc_applied_rev, \
     state AS hc_state, overridden AS hc_overridden FROM host_config) hc ON hc.hc_agent = hosts.agent_id";

/// `hosts` 的欄位（[`DbHost`] 的順序；第一欄 host_id 未使用）
pub const HOST_COLUMNS: &str = "host_id, agent_id, hostname, display_name, agent_version, proto, \
     host_fingerprint, tags, os, report_interval_ms, peer, identity_verified, first_seen, \
     connected_at, last_seen, offline_after_ms, last_report, source, cert_rejected_at, \
     cert_rejection, host_group";

fn host_of(r: &Row) -> PgResult<DbHost> {
    let text = |i: usize| r.get::<_, Option<String>>(i);
    let arc = |s: Option<String>| s.map(Arc::<str>::from);
    let tags: Vec<String> = text(7)?
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let rejection = match (r.get::<_, Option<i64>>(18)?, text(19)?) {
        (Some(at), Some(json)) => serde_json::from_str::<StoredRejection>(&json)
            .ok()
            .map(|s| CertRejection {
                at: DateTime::from_timestamp_millis(at).unwrap_or_default(),
                reason: RejectionReason::parse(&s.reason).unwrap_or(RejectionReason::Other),
                serial: s.serial,
                peer: s.peer.and_then(|p| p.parse().ok()),
                count: s.count,
            }),
        _ => None,
    };
    Ok(DbHost {
        agent_id: Arc::from(r.get::<_, String>(1)?),
        hostname: Arc::from(r.get::<_, String>(2)?),
        display_name: arc(text(3)?),
        agent_version: arc(text(4)?),
        proto: r.get(5)?,
        host_fingerprint: arc(text(6)?),
        tags: tags.into(),
        os: text(8)?
            .and_then(|t| serde_json::from_str(&t).ok())
            .map(Arc::new),
        report_interval_ms: r.get(9)?,
        peer: arc(text(10)?),
        identity_verified: r.get(11)?,
        first_seen: r.get(12)?,
        connected_at: r.get(13)?,
        last_seen: r.get(14)?,
        offline_after_ms: r.get(15)?,
        last_report: text(16)?
            .and_then(|t| serde_json::from_str(&t).ok())
            .map(Arc::new),
        imported: text(17)?.as_deref() == Some(HOST_SOURCE_IMPORT),
        rejection,
        host_group: arc(text(20)?),
        boot_time: r.get(21)?,
        inventory: {
            let s = InventorySummary {
                os_family: text(22)?,
                cpu_model: text(23)?,
                cpu_logical_cores: r.get(24)?,
                memory_total: r.get(25)?,
                vendor: text(26)?,
                product: text(27)?,
                serial: text(28)?,
            };
            // 有資產時 os_family 等至少一欄有值；全部為 NULL 視為沒有資產
            (s != InventorySummary::default()).then(|| Arc::new(s))
        },
        series_limited_at: r.get(29)?,
        install: {
            let i = kairos_proto::InstallInfo {
                method: text(30)?,
                upgrade: text(31)?,
                config: text(32)?,
                external_url: text(37)?,
            };
            (i != kairos_proto::InstallInfo::default()).then(|| Arc::new(i))
        },
        config: match r.get::<_, Option<i64>>(33)? {
            Some(desired_rev) => Some(Arc::new(crate::remote::desired::ConfigStateInput {
                desired_rev,
                applied_rev: r.get::<_, Option<i64>>(34)?.unwrap_or(0),
                state: text(35)?,
                overridden: crate::remote::desired::has_overridden(text(36)?.as_deref()),
            })),
            None => None,
        },
    })
}

/// 清冊中所有主機（原本在 `api/hosts.rs`）
pub fn load_hosts(conn: &PgConn) -> PgResult<Vec<DbHost>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {HOST_COLUMNS}, {HOST_EXTRA_COLUMNS} FROM hosts {HOST_EXTRA_JOIN} ORDER BY hosts.host_id"
    ))?;
    let rows = stmt.query_map((), host_of)?;
    rows.collect()
}

/// 清冊中的一台主機（原本在 `api/hosts.rs`）
pub fn load_host(conn: &PgConn, agent_id: &str) -> PgResult<Option<DbHost>> {
    conn.query_row(
        &format!(
            "SELECT {HOST_COLUMNS}, {HOST_EXTRA_COLUMNS} FROM hosts {HOST_EXTRA_JOIN} WHERE agent_id = ?1"
        ),
        [agent_id],
        host_of,
    )
    .optional()
}

/// 清冊中最大的 host_id（原本在 `api/hosts.rs`）
pub fn max_host_id(conn: &PgConn) -> PgResult<i64> {
    conn.query_row("SELECT coalesce(max(host_id), 0) FROM hosts", (), |r| {
        r.get(0)
    })
}

/// 這台主機最新一張憑證（原本在 `api/hosts.rs` 的 `certificate_summary`）
pub fn latest_cert_of(conn: &PgConn, agent_id: &str) -> PgResult<Option<(i64, Option<i64>)>> {
    conn.query_row(
        "SELECT not_after, revoked_at FROM pki_certificates WHERE agent_id = ?1 \
         ORDER BY issued_at DESC LIMIT 1",
        [agent_id],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
    )
    .optional()
}
