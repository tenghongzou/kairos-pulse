//! 稽核雜湊鏈的 SQLite 實作（[`crate::audit`] 的 [`AuditLog`]）。SQL 原封不動自 `audit.rs` 搬來。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::Arc;

use super::conn::{AsConn, forward};
use crate::audit::{
    Audit, AuditFilter, AuditKey, AuditLog, AuditRow, ChainBreak, GENESIS, HmacReport, META_ANCHOR,
    META_HEAD, META_HMAC, META_LEGACY_SEALED, VerifyReport, compute_hash, ct_eq_str,
    key_for_db_path, truncate,
};

/// 這個連線的資料庫所在資料目錄的金鑰（記憶體中的資料庫、沒有金鑰檔或無法讀取時為 `None`）
pub fn key_for(conn: &Connection) -> Option<Arc<AuditKey>> {
    key_for_db_path(conn.path())
}

impl<C: AsConn> AuditLog for C {
    forward! {
        fn audit_append(&self, at_ms: i64, a: &Audit<'_>) -> i64 => append;
        fn audit_hmac_since(&self) -> Option<(i64, String)> => hmac_since;
        fn audit_tail(&self) -> Option<(i64, Option<String>, Option<String>)> => tail;
        fn audit_seal_legacy(&self) -> usize => seal_legacy;
        fn audit_purge(&self, before_ms: i64, now_ms: i64) -> usize => purge;
        fn audit_verify(&self) -> VerifyReport => verify;
        fn audit_query(&self, f: &AuditFilter, limit: usize) -> (Vec<AuditRow>, u64) => query;
        fn audit_export_batch(&self, f: &AuditFilter, limit: usize) -> Vec<AuditRow> => export_batch;
    }
}

/// 鏈尾；沒有紀錄時為 `None`（原本在 `audit_monitor.rs`）
#[allow(clippy::type_complexity)]
pub fn tail(conn: &Connection) -> rusqlite::Result<Option<(i64, Option<String>, Option<String>)>> {
    conn.query_row(
        "SELECT id, hash, hmac FROM audit_events ORDER BY id DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

/// `meta.audit_hmac`：(since_id, key_id)
pub fn hmac_since(conn: &Connection) -> rusqlite::Result<Option<(i64, String)>> {
    Ok(meta_get(conn, META_HMAC)?.and_then(|v| {
        Some((
            v.get("since_id")?.as_i64()?,
            v.get("key_id")?.as_str()?.to_string(),
        ))
    }))
}

const COLUMNS: &str = "id, at, actor, actor_ip, category, action, object, result, code, details, prev_hash, hash, hmac";

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRow> {
    Ok(AuditRow {
        id: r.get(0)?,
        at: r.get(1)?,
        actor: r.get(2)?,
        actor_ip: r.get(3)?,
        category: r.get(4)?,
        action: r.get(5)?,
        object: r.get(6)?,
        result: r.get(7)?,
        code: r.get(8)?,
        details: r.get(9)?,
        prev_hash: r.get(10)?,
        hash: r.get(11)?,
        hmac: r.get(12)?,
    })
}

fn meta_get(conn: &Connection, key: &str) -> rusqlite::Result<Option<Value>> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
}

fn meta_set(conn: &Connection, key: &str, value: &Value) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value.to_string()],
    )?;
    Ok(())
}

/// 被清除的最後一筆（沒有清除過時為 `None`）：(id, hash)
pub fn anchor(conn: &Connection) -> rusqlite::Result<Option<(i64, String)>> {
    Ok(meta_get(conn, META_ANCHOR)?
        .and_then(|v| Some((v.get("id")?.as_i64()?, v.get("hash")?.as_str()?.to_string()))))
}

/// 在交易中執行 `f`：已在交易中時直接執行，否則以 `BEGIN IMMEDIATE` 包起來
pub(crate) fn in_write_tx<R>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> rusqlite::Result<R>,
) -> rusqlite::Result<R> {
    if !conn.is_autocommit() {
        return f(conn);
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f(conn) {
        Ok(r) => {
            conn.execute_batch("COMMIT")?;
            Ok(r)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// 寫入一筆稽核事件並接上雜湊鏈（資料目錄有金鑰檔時另加 HMAC）；回傳 id
pub fn append(conn: &Connection, at_ms: i64, a: &Audit<'_>) -> rusqlite::Result<i64> {
    let key = key_for(conn);
    append_with_key(conn, at_ms, a, key.as_deref())
}

/// [`append`]，指定 HMAC 的金鑰（`None`：不加 HMAC；已開始 HMAC 時這一筆在驗證時為 `missing_hmac`）
pub fn append_with_key(
    conn: &Connection,
    at_ms: i64,
    a: &Audit<'_>,
    key: Option<&AuditKey>,
) -> rusqlite::Result<i64> {
    in_write_tx(conn, |conn| {
        conn.execute(
            "INSERT INTO audit_events (at, actor, actor_ip, category, action, object, result, code, details)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                at_ms,
                truncate(a.actor, 200),
                a.actor_ip,
                a.category,
                a.action,
                a.object.map(|o| truncate(o, 512)),
                a.result,
                a.code,
                a.details.as_ref().map(Value::to_string)
            ],
        )?;
        let id = conn.last_insert_rowid();
        if meta_get(conn, META_LEGACY_SEALED)?.is_none() {
            // 升級後第一次寫入：連同升級前的紀錄（與這一筆）依 id 順序補上
            seal_legacy_in_tx(conn)?;
        } else {
            seal_one(conn, id)?;
        }
        if let Some(k) = key {
            sign_one(conn, id, k)?;
        }
        Ok(id)
    })
}

/// 剛寫入的這一筆加上 HMAC；第一次時記下起點。金鑰與起點記錄的不同時不簽（驗證時以
/// `hmac_key_mismatch` 指出，不用另一把金鑰把鏈「接續」下去）
fn sign_one(conn: &Connection, id: i64, key: &AuditKey) -> rusqlite::Result<()> {
    match hmac_since(conn)? {
        Some((_, kid)) if kid != key.key_id() => return Ok(()),
        Some(_) => {}
        None => meta_set(
            conn,
            META_HMAC,
            &json!({ "since_id": id, "key_id": key.key_id(), "started_at": chrono::Utc::now().timestamp_millis() }),
        )?,
    }
    let hash: Option<String> =
        conn.query_row("SELECT hash FROM audit_events WHERE id = ?1", [id], |r| {
            r.get(0)
        })?;
    let Some(hash) = hash else {
        return Ok(());
    };
    conn.execute(
        "UPDATE audit_events SET hmac = ?2 WHERE id = ?1",
        params![id, key.record_mac(&hash)],
    )?;
    let max: i64 = conn.query_row("SELECT max(id) FROM audit_events", [], |r| r.get(0))?;
    if max == id {
        meta_set(
            conn,
            META_HEAD,
            &json!({"id": id, "hash": hash, "mac": key.head_mac(id, &hash)}),
        )?;
    }
    Ok(())
}

/// 只計算剛插入的這一筆：接在 id 比它小、有雜湊的最後一筆之後（中間若有沒有雜湊的紀錄，驗證時
/// 以 `missing_hash` 指出，不會被接進鏈中）
fn seal_one(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    let row = conn.query_row(
        &format!("SELECT {COLUMNS} FROM audit_events WHERE id = ?1"),
        [id],
        row_of,
    )?;
    let prev: String = match conn
        .query_row(
            "SELECT hash FROM audit_events WHERE id < ?1 AND hash IS NOT NULL ORDER BY id DESC LIMIT 1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        Some(h) => h,
        None => anchor(conn)?
            .map(|(_, h)| h)
            .unwrap_or_else(|| GENESIS.to_string()),
    };
    let h = compute_hash(&prev, &row);
    conn.execute(
        "UPDATE audit_events SET prev_hash = ?2, hash = ?3 WHERE id = ?1",
        params![id, prev, h],
    )?;
    meta_set(conn, META_HEAD, &json!({"id": id, "hash": h}))
}

/// 升級前（schema v6 之前）寫入、沒有雜湊的紀錄依 id 順序補上雜湊（只做一次；之後是 no-op）；
/// 回傳補上的筆數
pub fn seal_legacy(conn: &Connection) -> rusqlite::Result<usize> {
    if meta_get(conn, META_LEGACY_SEALED)?.is_some() {
        return Ok(0);
    }
    in_write_tx(conn, |conn| {
        if meta_get(conn, META_LEGACY_SEALED)?.is_some() {
            return Ok(0);
        }
        seal_legacy_in_tx(conn)
    })
}

fn seal_legacy_in_tx(conn: &Connection) -> rusqlite::Result<usize> {
    let n = seal_null_rows(conn)?;
    let through: Option<i64> =
        conn.query_row("SELECT max(id) FROM audit_events", [], |r| r.get(0))?;
    meta_set(
        conn,
        META_LEGACY_SEALED,
        &json!({ "through_id": through, "sealed": n }),
    )?;
    Ok(n)
}

fn seal_null_rows(conn: &Connection) -> rusqlite::Result<usize> {
    let pending: Vec<AuditRow> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM audit_events WHERE hash IS NULL ORDER BY id"
        ))?;
        stmt.query_map([], row_of)?
            .collect::<rusqlite::Result<_>>()?
    };
    let Some(first) = pending.first() else {
        return Ok(0);
    };
    let mut prev: String = match conn
        .query_row(
            "SELECT hash FROM audit_events WHERE id < ?1 AND hash IS NOT NULL ORDER BY id DESC LIMIT 1",
            [first.id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        Some(h) => h,
        None => anchor(conn)?
            .map(|(_, h)| h)
            .unwrap_or_else(|| GENESIS.to_string()),
    };
    let mut stmt =
        conn.prepare("UPDATE audit_events SET prev_hash = ?2, hash = ?3 WHERE id = ?1")?;
    let mut last = None;
    for r in &pending {
        let h = compute_hash(&prev, r);
        stmt.execute(params![r.id, prev, h])?;
        last = Some((r.id, h.clone()));
        prev = h;
    }
    // 只在最新一筆是這次補上的時候更新 head（補的是較舊的紀錄時 head 不變）
    if let Some((id, h)) = last {
        let max: i64 = conn.query_row("SELECT max(id) FROM audit_events", [], |r| r.get(0))?;
        if max == id {
            meta_set(conn, META_HEAD, &json!({"id": id, "hash": h}))?;
        }
    }
    Ok(pending.len())
}

/// 依保留期限清除 `before_ms` 之前的紀錄，並把被清除的最後一筆記為新的錨點；回傳清除的筆數
pub fn purge(conn: &Connection, before_ms: i64, now_ms: i64) -> rusqlite::Result<usize> {
    in_write_tx(conn, |conn| {
        if meta_get(conn, META_LEGACY_SEALED)?.is_none() {
            seal_legacy_in_tx(conn)?;
        }
        // 只清除「從頭開始、時間早於門檻」的連續一段，鏈的開頭只會往後移
        let last: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, hash FROM audit_events WHERE id <= (
                    SELECT coalesce(min(id), (SELECT max(id) FROM audit_events) + 1) - 1
                    FROM audit_events WHERE at >= ?1)
                 ORDER BY id DESC LIMIT 1",
                [before_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, hash)) = last else {
            return Ok(0);
        };
        let n = conn.execute("DELETE FROM audit_events WHERE id <= ?1", [id])?;
        if n > 0 {
            meta_set(
                conn,
                META_ANCHOR,
                &json!({"id": id, "hash": hash, "purged_at": now_ms, "purged": n}),
            )?;
            // 全部清除：最新一筆就是錨點（下一筆接在它之後）
            let left: i64 =
                conn.query_row("SELECT count(*) FROM audit_events", [], |r| r.get(0))?;
            if left == 0 {
                conn.execute("DELETE FROM meta WHERE key = ?1", [META_HEAD])?;
            }
        }
        Ok(n)
    })
}

/// 從頭到尾重新計算雜湊鏈，回報第一個斷點（不修改任何資料）。所有讀取在同一個讀取交易（同一個
/// 快照）中進行：驗證期間其他連線寫入的紀錄不會造成假的斷點
///
/// HMAC 的金鑰依資料庫路徑取得（[`key_for`]）
pub fn verify(conn: &Connection) -> rusqlite::Result<VerifyReport> {
    let key = key_for(conn);
    verify_with_key(conn, key.as_deref())
}

/// [`verify`]，指定 HMAC 的金鑰
pub fn verify_with_key(
    conn: &Connection,
    key: Option<&AuditKey>,
) -> rusqlite::Result<VerifyReport> {
    if !conn.is_autocommit() {
        return verify_in_snapshot(conn, key);
    }
    conn.execute_batch("BEGIN")?;
    let r = verify_in_snapshot(conn, key);
    let _ = conn.execute_batch(if r.is_ok() { "COMMIT" } else { "ROLLBACK" });
    r
}

fn verify_in_snapshot(conn: &Connection, key: Option<&AuditKey>) -> rusqlite::Result<VerifyReport> {
    let anchor = anchor(conn)?;
    let head_meta = meta_get(conn, META_HEAD)?;
    let head = head_meta
        .as_ref()
        .and_then(|v| Some((v.get("id")?.as_i64()?, v.get("hash")?.as_str()?.to_string())));
    let head_mac = head_meta
        .as_ref()
        .and_then(|v| v.get("mac")?.as_str().map(str::to_string));
    let since = hmac_since(conn)?;
    // 能否逐筆驗證 HMAC：有起點、有金鑰且與起點記錄的相同
    let key_state = match (&since, key) {
        (None, _) => "not_started",
        (Some(_), None) => "key_unavailable",
        (Some((_, kid)), Some(k)) if kid != k.key_id() => "key_mismatch",
        (Some(_), Some(_)) => "verified",
    };
    let check_mac = if key_state == "verified" { key } else { None };
    let mut report = VerifyReport {
        checked: 0,
        first_id: None,
        last_id: None,
        last_hash: None,
        first_prev_hash: None,
        broken: None,
        hmac: HmacReport {
            since_id: since.as_ref().map(|s| s.0),
            key_id: since.as_ref().map(|s| s.1.clone()),
            status: key_state,
            verified: 0,
            chain_only: 0,
        },
    };
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM audit_events ORDER BY id"))?;
    let mut rows = stmt.query([])?;
    let mut prev: Option<String> = None;
    while let Some(r) = rows.next()? {
        let row = row_of(r)?;
        report.checked += 1;
        let brk = |reason| ChainBreak {
            id: row.id,
            reason,
            at: Some(row.at),
        };
        let (Some(p), Some(h)) = (&row.prev_hash, &row.hash) else {
            report.broken = Some(brk("missing_hash"));
            break;
        };
        match &prev {
            None => {
                report.first_id = Some(row.id);
                report.first_prev_hash = Some(p.clone());
                // 清除過：剩下的第一筆接在錨點之後（全部清除後 id 可能重新使用，不以 id 判斷）
                let expected = anchor.as_ref().map_or(GENESIS, |(_, h)| h.as_str());
                if p != expected {
                    report.broken = Some(brk("anchor_mismatch"));
                    break;
                }
            }
            Some(pv) if pv != p => {
                report.broken = Some(brk("prev_mismatch"));
                break;
            }
            _ => {}
        }
        if compute_hash(p, &row) != *h {
            report.broken = Some(brk("hash_mismatch"));
            break;
        }
        // HMAC：邊界之前不應該有（有表示邊界被移動）；之後每一筆都必須有，金鑰可用時逐筆比對
        match &since {
            Some((sid, _)) if row.id >= *sid => match (&row.hmac, check_mac) {
                (None, _) => {
                    report.broken = Some(brk("missing_hmac"));
                    break;
                }
                (Some(m), Some(k)) => {
                    if !ct_eq_str(m, &k.record_mac(h)) {
                        report.broken = Some(brk("hmac_mismatch"));
                        break;
                    }
                    report.hmac.verified += 1;
                }
                (Some(_), None) => {}
            },
            _ => {
                if row.hmac.is_some() {
                    report.broken = Some(brk("hmac_boundary_mismatch"));
                    break;
                }
                report.hmac.chain_only += 1;
            }
        }
        report.last_id = Some(row.id);
        report.last_hash = Some(h.clone());
        prev = Some(h.clone());
    }
    if report.broken.is_none()
        && let Some((hid, hh)) = head
    {
        let mut ok =
            report.last_id == Some(hid) && report.last_hash.as_deref() == Some(hh.as_str());
        // HMAC 開始之後的 head 必須帶正確的 mac（沒有金鑰無法把 head 改成較早的一筆）
        if ok
            && let (Some((sid, _)), Some(k)) = (&since, check_mac)
            && hid >= *sid
        {
            ok = head_mac
                .as_deref()
                .is_some_and(|m| ct_eq_str(m, &k.head_mac(hid, &hh)));
        }
        if !ok {
            report.broken = Some(ChainBreak {
                id: hid,
                reason: "head_mismatch",
                at: None,
            });
        }
    }
    // 雜湊鏈完整，但 HMAC 無法驗證：金鑰遺失或不符（不能確認紀錄沒有被整段重新計算）
    if report.broken.is_none()
        && let Some((sid, _)) = &since
        && report.last_id.is_some_and(|l| l >= *sid)
    {
        let reason = match key_state {
            "key_unavailable" => Some("hmac_key_unavailable"),
            "key_mismatch" => Some("hmac_key_mismatch"),
            _ => None,
        };
        if let Some(reason) = reason {
            report.broken = Some(ChainBreak {
                id: *sid,
                reason,
                at: None,
            });
        }
    }
    if key_state == "verified"
        && report
            .broken
            .as_ref()
            .is_some_and(|b| b.reason.contains("hmac") || b.reason == "head_mismatch")
    {
        report.hmac.status = "failed";
    }
    Ok(report)
}

/// 查詢條件的 WHERE 子句與參數（原本是 `AuditFilter::where_clause`）
fn where_clause(f: &AuditFilter) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value as V;
    let mut conds = Vec::new();
    let mut args: Vec<V> = Vec::new();
    let mut push = |cond: &str, v: V| {
        args.push(v);
        conds.push(cond.replace('?', &format!("?{}", args.len())));
    };
    if let Some(a) = &f.actor {
        push("actor = ?", V::Text(a.clone()));
    }
    if let Some(c) = &f.category {
        push("category = ?", V::Text(c.clone()));
    }
    if let Some(a) = &f.action {
        if a.ends_with('.') {
            // 前綴比對以 instr 實作（避免 LIKE 的萬用字元）
            push("instr(action, ?) = 1", V::Text(a.clone()));
        } else {
            push("action = ?", V::Text(a.clone()));
        }
    }
    if let Some(t) = &f.target {
        push(
            "instr(lower(coalesce(object, '')), ?) > 0",
            V::Text(t.to_lowercase()),
        );
    }
    if let Some(r) = &f.result {
        push("result = ?", V::Text(r.clone()));
    }
    if let Some(f) = f.from_ms {
        push("at >= ?", V::Integer(f));
    }
    if let Some(t) = f.to_ms {
        push("at < ?", V::Integer(t));
    }
    if let Some(b) = f.before_id {
        push("id < ?", V::Integer(b));
    }
    if let Some(a) = f.after_id {
        push("id > ?", V::Integer(a));
    }
    let w = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    (w, args)
}

/// 新到舊的 `limit` 筆，以及符合條件的總數
pub fn query(
    conn: &Connection,
    f: &AuditFilter,
    limit: usize,
) -> rusqlite::Result<(Vec<AuditRow>, u64)> {
    let (w, args) = where_clause(f);
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM audit_events{w} ORDER BY id DESC LIMIT {limit}"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), row_of)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let total_filter = AuditFilter {
        before_id: None,
        ..f.clone()
    };
    let (w, args) = where_clause(&total_filter);
    let total: u64 = conn.query_row(
        &format!("SELECT count(*) FROM audit_events{w}"),
        rusqlite::params_from_iter(args.iter()),
        |r| r.get::<_, i64>(0),
    )? as u64;
    Ok((rows, total))
}

/// 匯出：舊到新，每批最多 `limit` 筆（以 `after_id` 接續）
pub fn export_batch(
    conn: &Connection,
    f: &AuditFilter,
    limit: usize,
) -> rusqlite::Result<Vec<AuditRow>> {
    let (w, args) = where_clause(f);
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM audit_events{w} ORDER BY id LIMIT {limit}"
    ))?;
    stmt.query_map(rusqlite::params_from_iter(args.iter()), row_of)?
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::*;
    use crate::store::sqlite::{MIGRATIONS, migrate};

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        conn
    }

    fn ev<'a>(action: &'a str, object: Option<&'a str>) -> Audit<'a> {
        Audit {
            actor: "user:admin",
            actor_ip: Some("10.0.0.1"),
            category: "user",
            action,
            object,
            result: "ok",
            code: None,
            details: Some(json!({"reason": "測試"})),
        }
    }

    #[test]
    fn chain_links_every_record_and_detects_tampering() {
        let conn = db();
        for i in 0..5 {
            append(&conn, 1_000 + i, &ev("user.create", Some("bob"))).unwrap();
        }
        let r = verify(&conn).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(r.checked, 5);
        assert_eq!(r.first_prev_hash.as_deref(), Some(GENESIS));
        // 每筆的 prev_hash 是前一筆的 hash
        let rows = export_batch(&conn, &AuditFilter::default(), 100).unwrap();
        for w in rows.windows(2) {
            assert_eq!(w[1].prev_hash, w[0].hash);
        }
        // 修改內容
        conn.execute("UPDATE audit_events SET object = 'eve' WHERE id = 3", [])
            .unwrap();
        let r = verify(&conn).unwrap();
        assert_eq!(
            r.broken.as_ref().map(|b| (b.id, b.reason)),
            Some((3, "hash_mismatch"))
        );
        assert_eq!(r.json()["broken"]["product_code"], "KRS-E6201");
    }

    #[test]
    fn deletions_insertions_and_reordering_are_detected() {
        // 刪除中間一筆
        let conn = db();
        for i in 0..4 {
            append(&conn, i, &ev("a", None)).unwrap();
        }
        conn.execute("DELETE FROM audit_events WHERE id = 2", [])
            .unwrap();
        let b = verify(&conn).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (3, "prev_mismatch"));

        // 刪除最後一筆（head 不符）
        let conn = db();
        for i in 0..3 {
            append(&conn, i, &ev("a", None)).unwrap();
        }
        conn.execute("DELETE FROM audit_events WHERE id = 3", [])
            .unwrap();
        let b = verify(&conn).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (3, "head_mismatch"));

        // 人工插入沒有雜湊的紀錄
        let conn = db();
        append(&conn, 1, &ev("a", None)).unwrap();
        conn.execute(
            "INSERT INTO audit_events (at, actor, category, action, result) VALUES (2, 'x', 'user', 'b', 'ok')",
            [],
        )
        .unwrap();
        let b = verify(&conn).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (2, "missing_hash"));

        // 刪除第一筆（沒有清除的錨點）
        let conn = db();
        for i in 0..3 {
            append(&conn, i, &ev("a", None)).unwrap();
        }
        conn.execute("DELETE FROM audit_events WHERE id = 1", [])
            .unwrap();
        let b = verify(&conn).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (2, "anchor_mismatch"));
    }

    #[test]
    fn retention_purge_keeps_the_chain_verifiable() {
        let conn = db();
        for i in 0..10 {
            append(&conn, i * 1000, &ev("a", None)).unwrap();
        }
        assert_eq!(purge(&conn, 4_000, 99_000).unwrap(), 4);
        let r = verify(&conn).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(r.first_id, Some(5));
        assert_eq!(r.checked, 6);
        assert_eq!(anchor(&conn).unwrap().unwrap().0, 4);
        // 之後的寫入接在鏈尾
        append(&conn, 20_000, &ev("b", None)).unwrap();
        assert!(verify(&conn).unwrap().ok());
        // 沒有可清除的
        assert_eq!(purge(&conn, 0, 0).unwrap(), 0);
        // 全部清除後重新開始也能驗證（清除後、下一筆寫入前也是完整的）
        assert_eq!(purge(&conn, i64::MAX, 0).unwrap(), 7);
        assert!(verify(&conn).unwrap().ok());
        append(&conn, 30_000, &ev("c", None)).unwrap();
        assert!(verify(&conn).unwrap().ok());
    }

    #[test]
    fn records_written_before_v6_are_sealed_in_order() {
        let conn = db();
        // 模擬 v4／v5 寫入的紀錄（沒有雜湊）
        for i in 0..3 {
            conn.execute(
                "INSERT INTO audit_events (at, actor, category, action, result) VALUES (?1, 'system', 'pki', 'cert.issue', 'ok')",
                [i],
            )
            .unwrap();
        }
        assert!(!verify(&conn).unwrap().ok());
        // 第一次寫入時補上舊紀錄的雜湊
        append(&conn, 10, &ev("a", None)).unwrap();
        let r = verify(&conn).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(r.checked, 4);
        assert_eq!(seal_legacy(&conn).unwrap(), 0);
        // 之後直接插入資料庫的紀錄不會被下一次寫入「合法化」
        conn.execute(
            "INSERT INTO audit_events (at, actor, category, action, result) VALUES (11, 'mallory', 'user', 'user.create', 'ok')",
            [],
        )
        .unwrap();
        append(&conn, 12, &ev("b", None)).unwrap();
        assert_eq!(seal_legacy(&conn).unwrap(), 0);
        let b = verify(&conn).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (5, "missing_hash"));
    }

    #[test]
    fn legacy_rows_can_be_sealed_by_verification_before_any_write() {
        let conn = db();
        conn.execute(
            "INSERT INTO audit_events (at, actor, category, action, result) VALUES (1, 'system', 'pki', 'cert.issue', 'ok')",
            [],
        )
        .unwrap();
        assert_eq!(seal_legacy(&conn).unwrap(), 1);
        assert_eq!(seal_legacy(&conn).unwrap(), 0);
        assert!(verify(&conn).unwrap().ok());
    }

    /// 把整條鏈依目前的內容重新計算（模擬能寫入資料庫、但沒有 HMAC 金鑰的人）
    fn recompute_chain(conn: &Connection) {
        let rows = export_batch(conn, &AuditFilter::default(), 1000).unwrap();
        let mut prev = GENESIS.to_string();
        let mut last = None;
        for r in rows {
            let h = compute_hash(&prev, &r);
            conn.execute(
                "UPDATE audit_events SET prev_hash = ?2, hash = ?3 WHERE id = ?1",
                params![r.id, prev, h],
            )
            .unwrap();
            last = Some((r.id, h.clone()));
            prev = h;
        }
        let (id, h) = last.unwrap();
        meta_set(conn, META_HEAD, &json!({"id": id, "hash": h})).unwrap();
    }

    #[test]
    fn hmac_detects_a_recomputed_chain_that_the_hash_chain_alone_accepts() {
        let conn = db();
        let k = AuditKey::from_bytes(&[3u8; 32]);
        for i in 0..4 {
            append_with_key(&conn, i, &ev("user.create", Some("bob")), Some(&k)).unwrap();
        }
        let r = verify_with_key(&conn, Some(&k)).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(r.hmac.since_id, Some(1));
        assert_eq!(
            (r.hmac.status, r.hmac.verified, r.hmac.chain_only),
            ("verified", 4, 0)
        );
        assert_eq!(r.json()["hmac"]["status"], "verified");
        // 修改一筆並重新計算整條雜湊鏈：只看雜湊鏈時是完整的，HMAC 不符
        conn.execute("UPDATE audit_events SET object = 'eve' WHERE id = 3", [])
            .unwrap();
        recompute_chain(&conn);
        let r = verify_with_key(&conn, Some(&k)).unwrap();
        assert_eq!(
            r.broken.as_ref().map(|b| (b.id, b.reason)),
            Some((3, "hmac_mismatch"))
        );
        assert_eq!(r.hmac.status, "failed");
        // 偽造 HMAC 需要金鑰：用另一把金鑰簽也不符
        let other = AuditKey::from_bytes(&[4u8; 32]);
        assert_ne!(other.key_id(), k.key_id());
        let h: String = conn
            .query_row("SELECT hash FROM audit_events WHERE id = 3", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "UPDATE audit_events SET hmac = ?1 WHERE id = 3",
            [other.record_mac(&h)],
        )
        .unwrap();
        assert_eq!(
            verify_with_key(&conn, Some(&k))
                .unwrap()
                .broken
                .map(|b| b.reason),
            Some("hmac_mismatch")
        );
    }

    #[test]
    fn hmac_boundary_key_problems_and_head_truncation_are_reported() {
        let k = AuditKey::from_bytes(&[5u8; 32]);
        // 金鑰遺失：雜湊鏈完整，但驗證失敗（hmac_key_unavailable，指向起點）
        let conn = db();
        for i in 0..3 {
            append_with_key(&conn, i, &ev("a", None), Some(&k)).unwrap();
        }
        let b = verify_with_key(&conn, None).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (1, "hmac_key_unavailable"));
        // 金鑰不符：不以新的金鑰接續簽（之後的紀錄沒有 HMAC），驗證指出金鑰不符
        let other = AuditKey::from_bytes(&[6u8; 32]);
        let b = verify_with_key(&conn, Some(&other))
            .unwrap()
            .broken
            .unwrap();
        assert_eq!((b.id, b.reason), (1, "hmac_key_mismatch"));
        append_with_key(&conn, 9, &ev("b", None), Some(&other)).unwrap();
        let hm: Option<String> = conn
            .query_row("SELECT hmac FROM audit_events WHERE id = 4", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(hm, None);
        // 起點之後沒有 HMAC 的紀錄（例如金鑰無法讀取時寫入）：missing_hmac
        let b = verify_with_key(&conn, Some(&k)).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (4, "missing_hmac"));

        // 起點被往後移（想讓被改過的紀錄落在「只有雜湊鏈」的範圍）：hmac_boundary_mismatch
        let conn = db();
        for i in 0..3 {
            append_with_key(&conn, i, &ev("a", None), Some(&k)).unwrap();
        }
        meta_set(
            &conn,
            META_HMAC,
            &json!({"since_id": 3, "key_id": k.key_id()}),
        )
        .unwrap();
        let b = verify_with_key(&conn, Some(&k)).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (1, "hmac_boundary_mismatch"));

        // 刪除最後一筆並把 head 改成前一筆（雜湊是公開的，但 head 的 mac 需要金鑰）
        let conn = db();
        for i in 0..3 {
            append_with_key(&conn, i, &ev("a", None), Some(&k)).unwrap();
        }
        let h2: String = conn
            .query_row("SELECT hash FROM audit_events WHERE id = 2", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("DELETE FROM audit_events WHERE id = 3", [])
            .unwrap();
        meta_set(&conn, META_HEAD, &json!({"id": 2, "hash": h2})).unwrap();
        let b = verify_with_key(&conn, Some(&k)).unwrap().broken.unwrap();
        assert_eq!((b.id, b.reason), (2, "head_mismatch"));
        // 有正確 mac 的 head（重播先前的 head 需要外送的鏈尾雜湊才能發現）
        meta_set(
            &conn,
            META_HEAD,
            &json!({"id": 2, "hash": h2, "mac": k.head_mac(2, &h2)}),
        )
        .unwrap();
        assert!(verify_with_key(&conn, Some(&k)).unwrap().ok());
    }

    #[test]
    fn upgrade_from_a_0_6_chain_starts_hmac_at_a_clear_boundary() {
        // 0.6（schema v6）寫入的雜湊鏈：沒有 HMAC
        let conn = db();
        for i in 0..3 {
            append_with_key(&conn, i, &ev("a", None), None).unwrap();
        }
        let r = verify_with_key(&conn, None).unwrap();
        assert!(r.ok());
        assert_eq!(
            (r.hmac.status, r.hmac.since_id, r.hmac.chain_only),
            ("not_started", None, 3)
        );
        // 0.7 的服務有了金鑰：從下一筆起帶 HMAC，之前的 3 筆只有雜湊鏈
        let k = AuditKey::from_bytes(&[7u8; 32]);
        for i in 3..5 {
            append_with_key(&conn, i, &ev("b", None), Some(&k)).unwrap();
        }
        let r = verify_with_key(&conn, Some(&k)).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(r.hmac.since_id, Some(4));
        assert_eq!((r.hmac.verified, r.hmac.chain_only), (2, 3));
        // 保留期限清除到起點之後也能驗證
        assert_eq!(purge(&conn, 4, 99).unwrap(), 4);
        let r = verify_with_key(&conn, Some(&k)).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!((r.hmac.verified, r.hmac.chain_only), (1, 0));
    }

    #[test]
    fn a_0_6_database_file_and_chain_upgrade_to_v7_with_hmac() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-audit-v6-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut conn = Connection::open(dir.join(crate::store::sqlite::DB_FILE)).unwrap();
        assert_eq!(migrate(&mut conn, &MIGRATIONS[..6]).unwrap(), 6);
        // 0.6 寫入的雜湊鏈（v6 沒有 hmac 欄位；以 0.6 的算式補上雜湊與 head）
        let mut prev = GENESIS.to_string();
        for i in 1..=3i64 {
            conn.execute(
                "INSERT INTO audit_events (at, actor, category, action, result) VALUES (?1, 'system', 'system', 'system.start', 'ok')",
                [i],
            )
            .unwrap();
            let row = AuditRow {
                id: i,
                at: i,
                actor: "system".into(),
                actor_ip: None,
                category: "system".into(),
                action: "system.start".into(),
                object: None,
                result: "ok".into(),
                code: None,
                details: None,
                prev_hash: None,
                hash: None,
                hmac: None,
            };
            let h = compute_hash(&prev, &row);
            conn.execute(
                "UPDATE audit_events SET prev_hash = ?2, hash = ?3 WHERE id = ?1",
                params![i, prev, h],
            )
            .unwrap();
            prev = h;
        }
        meta_set(&conn, META_HEAD, &json!({"id": 3, "hash": prev})).unwrap();
        meta_set(
            &conn,
            META_LEGACY_SEALED,
            &json!({"through_id": 3, "sealed": 3}),
        )
        .unwrap();
        // 升級到 v7（0.8 起再到 v8）、服務建立金鑰後寫入：0.6 的 3 筆只有雜湊鏈，第 4 筆起有 HMAC
        assert_eq!(
            migrate(&mut conn, MIGRATIONS).unwrap(),
            crate::store::sqlite::SCHEMA_VERSION
        );
        let r = verify(&conn).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(r.hmac.status, "not_started");
        ensure_key(&dir).unwrap();
        append(&conn, 10, &ev("system.start", None)).unwrap();
        let r = verify(&conn).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(
            (
                r.checked,
                r.hmac.since_id,
                r.hmac.chain_only,
                r.hmac.verified
            ),
            (4, Some(4), 3, 1)
        );
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn key_file_is_created_once_found_by_database_path_and_private() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-audit-key-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 還沒有金鑰：CLI 不建立，寫入不帶 HMAC
        assert!(AuditKey::load(&dir).unwrap().is_none());
        let mut conn = Connection::open(dir.join(crate::store::sqlite::DB_FILE)).unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        assert!(key_for(&conn).is_none());
        append(&conn, 1, &ev("a", None)).unwrap();
        // 服務啟動：建立金鑰，之後依資料庫路徑找到同一把
        let k = ensure_key(&dir).unwrap();
        assert_eq!(std::fs::read(dir.join(KEY_FILE)).unwrap().len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        assert_eq!(key_for(&conn).unwrap().key_id(), k.key_id());
        assert_eq!(
            AuditKey::load_or_create(&dir).unwrap().key_id(),
            k.key_id(),
            "不覆寫既有的金鑰檔"
        );
        append(&conn, 2, &ev("b", None)).unwrap();
        let r = verify(&conn).unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(
            (r.hmac.since_id, r.hmac.verified, r.hmac.chain_only),
            (Some(2), 1, 1)
        );
        // 損毀的金鑰檔：拒絕使用
        std::fs::write(dir.join(KEY_FILE), b"short").unwrap();
        assert!(AuditKey::load(&dir).is_err());
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_inside_an_existing_transaction_and_queries() {
        let mut conn = db();
        let tx = conn.transaction().unwrap();
        append(&tx, 1, &ev("user.create", Some("Alice"))).unwrap();
        append(&tx, 2, &ev("user.disable", Some("bob"))).unwrap();
        tx.commit().unwrap();
        append(
            &conn,
            3,
            &Audit {
                actor: "anonymous",
                category: "login",
                action: "login.failure",
                object: Some("admin"),
                result: "failed",
                code: Some("KRS-E6001"),
                ..Audit::default()
            },
        )
        .unwrap();
        assert!(verify(&conn).unwrap().ok());
        let (rows, total) = query(&conn, &AuditFilter::default(), 2).unwrap();
        assert_eq!((rows.len(), total), (2, 3));
        assert_eq!(rows[0].id, 3);
        let f = AuditFilter {
            action: Some("user.".into()),
            ..AuditFilter::default()
        };
        assert_eq!(query(&conn, &f, 10).unwrap().1, 2);
        let f = AuditFilter {
            target: Some("ALI".into()),
            ..AuditFilter::default()
        };
        assert_eq!(
            query(&conn, &f, 10).unwrap().0[0].object.as_deref(),
            Some("Alice")
        );
        let f = AuditFilter {
            result: Some("failed".into()),
            actor: Some("anonymous".into()),
            from_ms: Some(3),
            to_ms: Some(4),
            ..AuditFilter::default()
        };
        assert_eq!(query(&conn, &f, 10).unwrap().1, 1);
        let f = AuditFilter {
            before_id: Some(3),
            ..AuditFilter::default()
        };
        let (rows, total) = query(&conn, &f, 10).unwrap();
        assert_eq!((rows.len(), total), (2, 3));
        // 匯出的每一行可以只靠自己的欄位重新計算雜湊
        for r in export_batch(&conn, &AuditFilter::default(), 10).unwrap() {
            let v = r.export_json();
            assert_eq!(v["type"], "event");
            assert_eq!(compute_hash(r.prev_hash.as_deref().unwrap(), &r), v["hash"]);
        }
        // 超長的對象被截短
        let long = "x".repeat(2000);
        append(&conn, 4, &ev("a", Some(&long))).unwrap();
        let (rows, _) = query(&conn, &AuditFilter::default(), 1).unwrap();
        assert!(rows[0].object.as_ref().unwrap().chars().count() <= 513);
    }
}
