//! [`crate::store::admin`] 的 SQLite 實作（[`AdminStore`]）。SQL 原封不動自呼叫端搬來。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

#[allow(unused_imports)]
use rusqlite::{Connection, OptionalExtension, params};

use super::conn::{AsConn, forward};
use crate::privacy::{EXPORT_MAX_ROWS, EXPORT_SCHEMA, PrivacySettings, is_time_column};
#[allow(unused_imports)]
use crate::store::admin::*;

impl<C: AsConn> AdminStore for C {
    forward! {
        fn personal_data(&self, agent_id: &str, settings: &PrivacySettings, now_ms: i64) -> Option<serde_json::Value> => personal_data;
        fn diag_counts(&self) -> (Option<i64>, Option<i64>, Option<i64>) => diag_counts;
        fn database_dir(&self) -> Option<std::path::PathBuf> => database_dir;
    }
}

/// 查詢結果轉成 JSON 物件的陣列：BLOB（雜湊、CSR、憑證的 DER、壓縮的 report）不輸出；JSON 文字欄位展開；
/// 時間欄位轉成 RFC 3339（UTC）
fn rows_json(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> rusqlite::Result<Vec<serde_json::Value>> {
    use rusqlite::types::ValueRef;
    use serde_json::{Value, json};
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().map(String::from).collect();
    let rows = stmt.query_map(p, |r| {
        let mut m = serde_json::Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match r.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(x) if is_time_column(n) && x > 100_000_000_000 => {
                    json!(crate::api::http::ts_ms(x))
                }
                ValueRef::Integer(x) => json!(x),
                ValueRef::Real(f) => json!(f),
                ValueRef::Text(t) => {
                    let text = String::from_utf8_lossy(t).into_owned();
                    let trimmed = text.trim_start();
                    if trimmed.starts_with('{') || trimmed.starts_with('[') {
                        serde_json::from_str(&text).unwrap_or(Value::String(text))
                    } else {
                        Value::String(text)
                    }
                }
                ValueRef::Blob(_) => continue,
            };
            m.insert(n.clone(), v);
        }
        Ok(Value::Object(m))
    })?;
    rows.collect()
}

/// 一台主機的個資（GDPR 第 15 條的存取請求、個資法第 3 條的查詢）。任何資料表都沒有這台主機時為 `None`。
///
/// 包含：清冊（主機名稱、顯示名稱、標籤、作業系統、來源位址、時間）、資產與變更歷史、最新的行程快照、
/// report 與彙總的筆數與時間範圍（指標數值以 `kairos reports export --agent` 取得）、告警事件與處理紀錄、
/// 註冊申請（不含 CSR 與輪詢憑據的雜湊）、憑證的簽發紀錄（不含 DER）、與這台主機有關的稽核紀錄
pub fn personal_data(
    conn: &Connection,
    agent_id: &str,
    settings: &PrivacySettings,
    now_ms: i64,
) -> rusqlite::Result<Option<serde_json::Value>> {
    use rusqlite::OptionalExtension;
    use serde_json::{Value, json};
    let host = rows_json(conn, "SELECT * FROM hosts WHERE agent_id = ?1", [agent_id])?
        .into_iter()
        .next();
    let host_id: Option<i64> = conn
        .query_row(
            "SELECT host_id FROM hosts WHERE agent_id = ?1",
            [agent_id],
            |r| r.get(0),
        )
        .optional()?;
    let requests = rows_json(
        conn,
        "SELECT * FROM enroll_requests WHERE agent_id = ?1 ORDER BY created_at LIMIT ?2",
        rusqlite::params![agent_id, EXPORT_MAX_ROWS],
    )?;
    let certificates = rows_json(
        conn,
        "SELECT serial, ca_id, agent_id, not_before, not_after, issued_at, issued_via, request_id,
                approved_by, first_used_at, revoked_at, revoke_effective_at, revocation_reason,
                revoked_by, revoke_comment
         FROM pki_certificates WHERE agent_id = ?1 ORDER BY issued_at",
        [agent_id],
    )?;
    let mut alerts = rows_json(
        conn,
        "SELECT * FROM alert_events WHERE agent_id = ?1 ORDER BY started_at LIMIT ?2",
        rusqlite::params![agent_id, EXPORT_MAX_ROWS],
    )?;
    for a in &mut alerts {
        let id = a["event_id"].as_str().unwrap_or_default().to_string();
        a["timeline"] = json!(rows_json(
            conn,
            "SELECT at, actor, action, details FROM alert_timeline WHERE event_id = ?1 ORDER BY id",
            [&id],
        )?);
    }
    if host.is_none() && requests.is_empty() && certificates.is_empty() && alerts.is_empty() {
        return Ok(None);
    }
    let (inventory, changes, processes, reports) = match host_id {
        Some(h) => {
            let inventory =
                rows_json(conn, "SELECT * FROM host_inventory WHERE host_id = ?1", [h])?
                    .into_iter()
                    .next();
            let changes = rows_json(
                conn,
                "SELECT changed_at, changes FROM inventory_changes WHERE host_id = ?1 ORDER BY id",
                [h],
            )?;
            let (count, first, last): (i64, Option<i64>, Option<i64>) = conn.query_row(
                "SELECT COUNT(*), MIN(ts), MAX(ts) FROM process_snapshots WHERE host_id = ?1",
                [h],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let latest: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT body FROM process_snapshots WHERE host_id = ?1 ORDER BY ts DESC LIMIT 1",
                    [h],
                    |r| r.get(0),
                )
                .optional()?;
            let processes = json!({
                "count": count,
                "first": first.map(crate::api::http::ts_ms),
                "last": last.map(crate::api::http::ts_ms),
                "latest": latest.and_then(|b| serde_json::from_slice::<Value>(&b).ok()),
            });
            let (n, first, last): (i64, Option<i64>, Option<i64>) = conn.query_row(
                "SELECT COUNT(*), MIN(ts), MAX(ts) FROM reports WHERE host_id = ?1",
                [h],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let count_of = |table: &str| -> rusqlite::Result<i64> {
                conn.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE host_id = ?1"),
                    [h],
                    |r| r.get(0),
                )
            };
            let reports = json!({
                "count": n,
                "first": first.map(crate::api::http::ts_ms),
                "last": last.map(crate::api::http::ts_ms),
                "rollup_5m_rows": count_of("rollup_5m")?,
                "rollup_1h_rows": count_of("rollup_1h")?,
                "export": format!("kairos reports export --agent {agent_id} --since 400d"),
            });
            (inventory, changes, processes, reports)
        }
        None => (None, Vec::new(), Value::Null, Value::Null),
    };
    // 稽核紀錄：對象是這台主機、它的申請或憑證，或執行者是這台主機（host:<agent_id 前 8 碼>）
    let mut objects: Vec<String> = vec![agent_id.to_string()];
    objects.extend(
        requests
            .iter()
            .filter_map(|r| r["request_id"].as_str().map(str::to_string)),
    );
    objects.extend(
        certificates
            .iter()
            .filter_map(|c| c["serial"].as_str().map(str::to_string)),
    );
    let actor = format!("host:{}", agent_id.chars().take(8).collect::<String>());
    let mut audit = Vec::new();
    for chunk in objects.chunks(200) {
        let marks = vec!["?"; chunk.len()].join(",");
        let mut params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        params.push(&actor);
        audit.extend(rows_json(
            conn,
            &format!(
                "SELECT id, at, actor, actor_ip, category, action, object, result, code, details
                 FROM audit_events WHERE object IN ({marks}) OR actor = ? ORDER BY id LIMIT {EXPORT_MAX_ROWS}"
            ),
            params.as_slice(),
        )?);
    }
    audit.sort_by_key(|a| a["id"].as_i64().unwrap_or_default());
    audit.dedup_by_key(|a| a["id"].as_i64().unwrap_or_default());
    // 集中設定的狀態與升級推送的紀錄（0.9.0）
    let remote = json!({
        "host_config": rows_json(conn, "SELECT * FROM host_config WHERE agent_id = ?1", [agent_id])?
            .into_iter()
            .next(),
        "rollout_hosts": match host_id {
            Some(h) => rows_json(
                conn,
                "SELECT * FROM rollout_hosts WHERE host_id = ?1 ORDER BY updated_at",
                [h],
            )?,
            None => Vec::new(),
        },
    });
    Ok(Some(json!({
        "schema": EXPORT_SCHEMA,
        "generated_at": crate::api::http::ts_ms(now_ms),
        "agent_id": agent_id,
        "notice": "這是 Kairos 保存的這台主機的資料（資料主體存取請求）。時間為 UTC；主機名稱、來源位址等欄位依 [privacy] 的設定保存（見 privacy）。Kairos 的日誌與備份不在這份匯出中（kairos/PRIVACY.md）。",
        "privacy": settings,
        "host": host,
        "inventory": inventory,
        "inventory_changes": changes,
        "process_snapshots": processes,
        "reports": reports,
        "alerts": alerts,
        "enroll_requests": requests,
        "certificates": certificates,
        "remote_management": remote,
        "audit_events": audit,
    })))
}

/// 診斷包的資料庫摘要（原本在 `diag.rs` 的 `database_summary`）
pub fn diag_counts(conn: &Connection) -> rusqlite::Result<(Option<i64>, Option<i64>, Option<i64>)> {
    let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).ok();
    Ok((
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .ok(),
        count("SELECT COUNT(*) FROM hosts"),
        count("SELECT COUNT(*) FROM hosts WHERE online = 1"),
    ))
}

/// 資料庫檔所在的目錄（原本在 `alerts_cli.rs`）
pub fn database_dir(conn: &Connection) -> rusqlite::Result<Option<std::path::PathBuf>> {
    Ok(conn.path().filter(|p| !p.is_empty()).and_then(|p| {
        std::path::Path::new(p)
            .parent()
            .map(std::path::Path::to_path_buf)
    }))
}
