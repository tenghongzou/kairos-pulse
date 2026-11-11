//! [`crate::store::admin`] 的 PostgreSQL 實作（[`AdminStore`]）。逐函式對照 `store/sqlite/admin.rs`。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

#[allow(unused_imports)]
use super::conn::{
    OptionalExtension, Params, PgConn, PgResult, Value as SqlValue, params, params_from_iter,
    pg_domain,
};
use crate::privacy::{EXPORT_MAX_ROWS, EXPORT_SCHEMA, PrivacySettings, is_time_column};
#[allow(unused_imports)]
use crate::store::admin::*;

pg_domain! { AdminStore {
    fn personal_data(&self, agent_id: &str, settings: &PrivacySettings, now_ms: i64) -> Option<serde_json::Value> => personal_data;
    fn diag_counts(&self) -> (Option<i64>, Option<i64>, Option<i64>) => diag_counts;
    fn database_dir(&self) -> Option<std::path::PathBuf> => database_dir;
} }

/// 目前 schema 中一張資料表的欄位名稱（依定義順序）。連線的介面不提供查詢結果的欄位名稱，`SELECT *`
/// 的匯出改成先查出欄位再明確列出
fn table_columns(conn: &PgConn, table: &str) -> PgResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT a.attname::text FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = current_schema() AND c.relname = ?1::text AND c.relkind = 'r'
           AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY a.attnum",
    )?;
    let rows = stmt.query_map([table], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// 「a, b, c」形式的欄位清單拆成名稱
fn column_names(list: &str) -> Vec<String> {
    list.split(',')
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

/// 查詢結果轉成 JSON 物件的陣列：BYTEA（雜湊、CSR、憑證的 DER、壓縮的 report）不輸出；JSON 文字欄位展開；
/// 時間欄位轉成 RFC 3339（UTC）。`names` 是查詢結果各欄位的名稱（與 SELECT 的順序相同）
fn rows_json(
    conn: &PgConn,
    names: &[String],
    sql: &str,
    p: impl Params,
) -> PgResult<Vec<serde_json::Value>> {
    use serde_json::{Value, json};
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(p, |r| {
        let mut m = serde_json::Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match r.get::<_, SqlValue>(i)? {
                SqlValue::Null => Value::Null,
                SqlValue::Integer(x) if is_time_column(n) && x > 100_000_000_000 => {
                    json!(crate::api::http::ts_ms(x))
                }
                SqlValue::Integer(x) => json!(x),
                SqlValue::Real(f) => json!(f),
                SqlValue::Text(text) => {
                    let trimmed = text.trim_start();
                    if trimmed.starts_with('{') || trimmed.starts_with('[') {
                        serde_json::from_str(&text).unwrap_or(Value::String(text))
                    } else {
                        Value::String(text)
                    }
                }
                SqlValue::Blob(_) | SqlValue::Array(_) => continue,
            };
            m.insert(n.clone(), v);
        }
        Ok(Value::Object(m))
    })?;
    rows.collect()
}

/// 明確列出欄位的查詢：`SELECT <columns> <tail>`
fn select_json(
    conn: &PgConn,
    columns: &str,
    tail: &str,
    p: impl Params,
) -> PgResult<Vec<serde_json::Value>> {
    rows_json(
        conn,
        &column_names(columns),
        &format!("SELECT {columns} {tail}"),
        p,
    )
}

/// 一張資料表的所有欄位（SQLite 版的 `SELECT * FROM <table> <tail>`）
fn table_json(
    conn: &PgConn,
    table: &str,
    tail: &str,
    p: impl Params,
) -> PgResult<Vec<serde_json::Value>> {
    let names = table_columns(conn, table)?;
    if names.is_empty() {
        return Err(super::conn::PgError::other(format!(
            "找不到資料表 {table} 的欄位"
        )));
    }
    let list = names
        .iter()
        .map(|n| format!("\"{}\"", n.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    rows_json(
        conn,
        &names,
        &format!("SELECT {list} FROM {table} {tail}"),
        p,
    )
}

/// 一台主機的個資（GDPR 第 15 條的存取請求、個資法第 3 條的查詢）。任何資料表都沒有這台主機時為 `None`。
///
/// 包含：清冊（主機名稱、顯示名稱、標籤、作業系統、來源位址、時間）、資產與變更歷史、最新的行程快照、
/// report 與彙總的筆數與時間範圍（指標數值以 `kairos reports export --agent` 取得）、告警事件與處理紀錄、
/// 註冊申請（不含 CSR 與輪詢憑據的雜湊）、憑證的簽發紀錄（不含 DER）、與這台主機有關的稽核紀錄
pub fn personal_data(
    conn: &PgConn,
    agent_id: &str,
    settings: &PrivacySettings,
    now_ms: i64,
) -> PgResult<Option<serde_json::Value>> {
    use serde_json::{Value, json};
    let host = table_json(conn, "hosts", "WHERE agent_id = ?1", [agent_id])?
        .into_iter()
        .next();
    let host_id: Option<i64> = conn
        .query_row(
            "SELECT host_id FROM hosts WHERE agent_id = ?1",
            [agent_id],
            |r| r.get(0),
        )
        .optional()?;
    let requests = table_json(
        conn,
        "enroll_requests",
        "WHERE agent_id = ?1 ORDER BY created_at LIMIT ?2",
        params![agent_id, EXPORT_MAX_ROWS],
    )?;
    let certificates = select_json(
        conn,
        "serial, ca_id, agent_id, not_before, not_after, issued_at, issued_via, request_id,
                approved_by, first_used_at, revoked_at, revoke_effective_at, revocation_reason,
                revoked_by, revoke_comment",
        "FROM pki_certificates WHERE agent_id = ?1 ORDER BY issued_at",
        [agent_id],
    )?;
    let mut alerts = table_json(
        conn,
        "alert_events",
        "WHERE agent_id = ?1 ORDER BY started_at LIMIT ?2",
        params![agent_id, EXPORT_MAX_ROWS],
    )?;
    for a in &mut alerts {
        let id = a["event_id"].as_str().unwrap_or_default().to_string();
        a["timeline"] = json!(select_json(
            conn,
            "at, actor, action, details",
            "FROM alert_timeline WHERE event_id = ?1 ORDER BY id",
            [&id],
        )?);
    }
    if host.is_none() && requests.is_empty() && certificates.is_empty() && alerts.is_empty() {
        return Ok(None);
    }
    let (inventory, changes, processes, reports) = match host_id {
        Some(h) => {
            let inventory = table_json(conn, "host_inventory", "WHERE host_id = ?1", [h])?
                .into_iter()
                .next();
            let changes = select_json(
                conn,
                "changed_at, changes",
                "FROM inventory_changes WHERE host_id = ?1 ORDER BY id",
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
            let count_of = |table: &str| -> PgResult<i64> {
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
        let p = params_from_iter(
            chunk
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(actor.as_str())),
        );
        audit.extend(select_json(
            conn,
            "id, at, actor, actor_ip, category, action, object, result, code, details",
            &format!(
                "FROM audit_events WHERE object IN ({marks}) OR actor = ? ORDER BY id LIMIT {EXPORT_MAX_ROWS}"
            ),
            p,
        )?);
    }
    audit.sort_by_key(|a| a["id"].as_i64().unwrap_or_default());
    audit.dedup_by_key(|a| a["id"].as_i64().unwrap_or_default());
    // 集中設定的狀態與升級推送的紀錄（0.9.0）
    let remote = json!({
        "host_config": table_json(conn, "host_config", "WHERE agent_id = ?1", [agent_id])?
            .into_iter()
            .next(),
        "rollout_hosts": match host_id {
            Some(h) => table_json(conn, "rollout_hosts", "WHERE host_id = ?1 ORDER BY updated_at", [h])?,
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

/// 診斷包的資料庫摘要（原本在 `diag.rs` 的 `database_summary`）：schema 版本（`meta` 的
/// `schema_version`；沒有這個鍵時視為 0，讀不到 `meta` 時 `None`）、主機數、線上主機數
pub fn diag_counts(conn: &PgConn) -> PgResult<(Option<i64>, Option<i64>, Option<i64>)> {
    let count = |sql: &str| conn.query_row(sql, (), |r| r.get::<_, i64>(0)).ok();
    let version = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            (),
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .ok()
        .map(|v| v.flatten().unwrap_or(0));
    Ok((
        version,
        count("SELECT COUNT(*) FROM hosts"),
        count("SELECT COUNT(*) FROM hosts WHERE online = 1"),
    ))
}

/// 節點本機的資料目錄（SQLite 版是資料庫檔所在的目錄；原本在 `alerts_cli.rs`）
pub fn database_dir(conn: &PgConn) -> PgResult<Option<std::path::PathBuf>> {
    Ok(conn
        .pool()
        .db_path_hint()
        .filter(|p| !p.is_empty())
        .and_then(|p| {
            std::path::Path::new(&p)
                .parent()
                .map(std::path::Path::to_path_buf)
        }))
}
