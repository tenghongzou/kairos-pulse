//! 稽核雜湊鏈的 PostgreSQL 實作（[`crate::audit`] 的 [`AuditLog`]）。逐函式對照 `store/sqlite/audit.rs`。
//!
//! 與 SQLite 版的差別（`kairos/HA.md` 第 12.1 節）：
//!
//! - **寫入者的序列化**：SQLite 靠寫入鎖；這裡每個會改動鏈的操作（寫入、補雜湊、保留期限清除）在交易中
//!   第一步取得交易層級的 advisory lock（[`LockKind::Audit`]），持有到交易結束，所以鏈的順序等於
//!   commit 的順序。
//! - **id 在持鎖期間明確指定**（`max(id) + 1`），不用 IDENTITY／序列：回復的交易不佔用 id，id 連續。
//! - **驗證**在 `REPEATABLE READ` 的唯讀交易（同一個快照）中進行。
//!
//! 雜湊與 HMAC 的算式（[`compute_hash`]、[`AuditKey`]）與 SQLite 版共用：兩個後端對同一筆紀錄算出相同的
//! `hash` 與 `hmac`。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use serde_json::{Value, json};
use std::sync::Arc;

use super::conn::{LockKind, OptionalExtension, PgConn, PgResult, Row, params, pg_domain};
use crate::audit::{
    Audit, AuditFilter, AuditKey, AuditLog, AuditRow, ChainBefore, ChainBreak, GENESIS, Head,
    HmacReport, META_ANCHOR, META_HEAD, META_HMAC, META_LEGACY_SEALED, VerifyReport, check_head,
    compute_hash, ct_eq_str, key_for_db_path, truncate,
};
use crate::store::db::TxMode;

/// 這個連線所屬節點的資料目錄的金鑰（沒有資料目錄、沒有金鑰檔或無法讀取時為 `None`）
pub fn key_for(conn: &PgConn) -> Option<Arc<AuditKey>> {
    key_for_db_path(conn.pool().db_path_hint().as_deref())
}

pg_domain! { AuditLog {
    fn audit_append(&self, at_ms: i64, a: &Audit<'_>) -> i64 => append;
    fn audit_hmac_since(&self) -> Option<(i64, String)> => hmac_since;
    fn audit_tail(&self) -> Option<(i64, Option<String>, Option<String>)> => tail;
    fn audit_seal_legacy(&self) -> usize => seal_legacy;
    fn audit_purge(&self, before_ms: i64, now_ms: i64) -> usize => purge;
    fn audit_verify(&self) -> VerifyReport => verify;
    fn audit_query(&self, f: &AuditFilter, limit: usize) -> (Vec<AuditRow>, u64) => query;
    fn audit_export_batch(&self, f: &AuditFilter, limit: usize) -> Vec<AuditRow> => export_batch;
} }

/// 鏈尾；沒有紀錄時為 `None`（原本在 `audit_monitor.rs`）
#[allow(clippy::type_complexity)]
pub fn tail(conn: &PgConn) -> PgResult<Option<(i64, Option<String>, Option<String>)>> {
    conn.query_row(
        "SELECT id, hash, hmac FROM audit_events ORDER BY id DESC LIMIT 1",
        (),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

/// `meta.audit_hmac`：(since_id, key_id)
pub fn hmac_since(conn: &PgConn) -> PgResult<Option<(i64, String)>> {
    Ok(meta_get(conn, META_HMAC)?.and_then(|v| {
        Some((
            v.get("since_id")?.as_i64()?,
            v.get("key_id")?.as_str()?.to_string(),
        ))
    }))
}

const COLUMNS: &str = "id, at, actor, actor_ip, category, action, object, result, code, details, prev_hash, hash, hmac";

fn row_of(r: &Row) -> PgResult<AuditRow> {
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

/// `meta` 的 JSON 值（以 TEXT 保存）
fn meta_get(conn: &PgConn, key: &str) -> PgResult<Option<Value>> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten();
    Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
}

fn meta_set(conn: &PgConn, key: &str, value: &Value) -> PgResult<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value.to_string()],
    )?;
    Ok(())
}

/// 被清除的最後一筆（沒有清除過時為 `None`）：(id, hash)
pub fn anchor(conn: &PgConn) -> PgResult<Option<(i64, String)>> {
    Ok(meta_get(conn, META_ANCHOR)?
        .and_then(|v| Some((v.get("id")?.as_i64()?, v.get("hash")?.as_str()?.to_string()))))
}

/// 在交易中執行 `f`：已在交易中時直接執行，否則開一個序列化的寫入交易（[`TxMode::Immediate`]）包起來。
/// `f` 失敗時交易回復
pub(crate) fn in_write_tx<R>(conn: &PgConn, f: impl FnOnce(&PgConn) -> PgResult<R>) -> PgResult<R> {
    if conn.in_transaction() {
        return f(conn);
    }
    let tx = conn.begin_tx(TxMode::Immediate)?;
    let r = f(&tx)?;
    tx.commit()?;
    Ok(r)
}

/// 寫入一筆稽核事件並接上雜湊鏈（資料目錄有金鑰檔時另加 HMAC）；回傳 id
pub fn append(conn: &PgConn, at_ms: i64, a: &Audit<'_>) -> PgResult<i64> {
    let key = key_for(conn);
    append_with_key(conn, at_ms, a, key.as_deref())
}

/// [`append`]，指定 HMAC 的金鑰（`None`：不加 HMAC；已開始 HMAC 時這一筆在驗證時為 `missing_hmac`）
pub fn append_with_key(
    conn: &PgConn,
    at_ms: i64,
    a: &Audit<'_>,
    key: Option<&AuditKey>,
) -> PgResult<i64> {
    in_write_tx(conn, |conn| {
        // 第一步：序列化所有稽核寫入者，直到交易結束（之後讀到的鏈尾不會再被別人接走）
        conn.lock_in_tx(LockKind::Audit)?;
        let before = chain_before(conn)?;
        // 持鎖期間指定 id：回復的交易不佔用 id（IDENTITY／序列會留下空號）
        let id: i64 = conn.query_row(
            "SELECT COALESCE(MAX(id), 0) + 1 FROM audit_events",
            (),
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO audit_events (id, at, actor, actor_ip, category, action, object, result, code, details)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
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
        if meta_get(conn, META_LEGACY_SEALED)?.is_none() {
            // 升級後第一次寫入：連同升級前的紀錄（與這一筆）依 id 順序補上
            seal_legacy_in_tx(conn)?;
        } else {
            seal_one(conn, id)?;
        }
        if let Some(k) = key {
            sign_one(conn, id, k, &before)?;
        }
        Ok(id)
    })
}

/// 寫入或清除之前鏈的狀態（持有稽核鎖的交易中讀取）
fn chain_before(conn: &PgConn) -> PgResult<ChainBefore> {
    Ok(ChainBefore {
        head: meta_get(conn, META_HEAD)?,
        tail: tail(conn)?.map(|(id, hash, _)| (id, hash)),
        anchor: anchor(conn)?,
        since: hmac_since(conn)?,
    })
}

/// head 與寫入前的狀態不一致時放回原本的值（[`ChainBefore::can_advance`]）
fn restore_head(conn: &PgConn, head: Option<&Value>) -> PgResult<()> {
    match head {
        Some(v) => meta_set(conn, META_HEAD, v),
        None => conn
            .execute("DELETE FROM meta WHERE key = ?1", [META_HEAD])
            .map(|_| ()),
    }
}

/// 剛寫入的這一筆加上 HMAC；第一次時記下起點。金鑰與起點記錄的不同時不簽（驗證時以
/// `hmac_key_mismatch` 指出，不用另一把金鑰把鏈「接續」下去）
fn sign_one(conn: &PgConn, id: i64, key: &AuditKey, before: &ChainBefore) -> PgResult<()> {
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
    let max: i64 = conn.query_row("SELECT max(id) FROM audit_events", (), |r| r.get(0))?;
    if max == id {
        if before.can_advance(key) {
            meta_set(
                conn,
                META_HEAD,
                &Head::v2_json(key, (id, &hash), anchor(conn)?.as_ref()),
            )?;
        } else {
            // 寫入前 head 就與鏈尾或錨點不一致：這一筆照常寫入並簽章，head 保留原狀，驗證會持續指出
            log::debug!("稽核鏈的 head 與寫入前的鏈尾或錨點不一致，不更新 head（第 {id} 筆）");
            restore_head(conn, before.head.as_ref())?;
        }
    }
    Ok(())
}

/// 只計算剛插入的這一筆：接在 id 比它小、有雜湊的最後一筆之後（中間若有沒有雜湊的紀錄，驗證時
/// 以 `missing_hash` 指出，不會被接進鏈中）
fn seal_one(conn: &PgConn, id: i64) -> PgResult<()> {
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
/// 回傳補上的筆數。在持有稽核鎖的交易中執行（`kairos/HA.md` 第 12.1 節）
pub fn seal_legacy(conn: &PgConn) -> PgResult<usize> {
    if meta_get(conn, META_LEGACY_SEALED)?.is_some() {
        return Ok(0);
    }
    in_write_tx(conn, |conn| {
        conn.lock_in_tx(LockKind::Audit)?;
        // 取得鎖之後再確認一次：等待期間可能已由另一個寫入者補上
        if meta_get(conn, META_LEGACY_SEALED)?.is_some() {
            return Ok(0);
        }
        seal_legacy_in_tx(conn)
    })
}

/// 呼叫端必須已持有稽核鎖
fn seal_legacy_in_tx(conn: &PgConn) -> PgResult<usize> {
    let n = seal_null_rows(conn)?;
    let through: Option<i64> =
        conn.query_row("SELECT max(id) FROM audit_events", (), |r| r.get(0))?;
    meta_set(
        conn,
        META_LEGACY_SEALED,
        &json!({ "through_id": through, "sealed": n }),
    )?;
    Ok(n)
}

fn seal_null_rows(conn: &PgConn) -> PgResult<usize> {
    let pending: Vec<AuditRow> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM audit_events WHERE hash IS NULL ORDER BY id"
        ))?;
        stmt.query_map((), row_of)?.collect::<PgResult<_>>()?
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
        let max: i64 = conn.query_row("SELECT max(id) FROM audit_events", (), |r| r.get(0))?;
        if max == id {
            meta_set(conn, META_HEAD, &json!({"id": id, "hash": h}))?;
        }
    }
    Ok(pending.len())
}

/// 依保留期限清除 `before_ms` 之前的紀錄，並把被清除的最後一筆記為新的錨點；回傳清除的筆數。
/// 會改動鏈（刪除開頭、更新錨點），因此同樣先取得稽核鎖
pub fn purge(conn: &PgConn, before_ms: i64, now_ms: i64) -> PgResult<usize> {
    let key = key_for(conn);
    purge_with_key(conn, before_ms, now_ms, key.as_deref())
}

/// [`purge`]，指定 HMAC 的金鑰：HMAC 已開始且金鑰相符時，新的錨點連同鏈尾一起簽進 head
pub fn purge_with_key(
    conn: &PgConn,
    before_ms: i64,
    now_ms: i64,
    key: Option<&AuditKey>,
) -> PgResult<usize> {
    in_write_tx(conn, |conn| {
        conn.lock_in_tx(LockKind::Audit)?;
        if meta_get(conn, META_LEGACY_SEALED)?.is_none() {
            seal_legacy_in_tx(conn)?;
        }
        let before = chain_before(conn)?;
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
                conn.query_row("SELECT count(*) FROM audit_events", (), |r| r.get(0))?;
            match key.filter(|k| before.signs_with(k)) {
                // HMAC 已開始：錨點連同鏈尾簽進 head。清除前 head 就不一致時保留原狀（驗證會指出）
                Some(k) => {
                    if before.can_advance(k) {
                        let tail = match (&before.tail, left) {
                            (Some((tid, Some(th))), 1..) => (*tid, th.as_str()),
                            _ => (id, hash.as_str()),
                        };
                        let new_anchor = (id, hash.clone());
                        meta_set(conn, META_HEAD, &Head::v2_json(k, tail, Some(&new_anchor)))?;
                    }
                }
                None => {
                    if left == 0 {
                        conn.execute("DELETE FROM meta WHERE key = ?1", [META_HEAD])?;
                    }
                }
            }
        }
        Ok(n)
    })
}

/// 從頭到尾重新計算雜湊鏈，回報第一個斷點（不修改任何資料）。所有讀取在同一個 `REPEATABLE READ` 的
/// 唯讀交易（同一個快照）中進行：驗證期間其他連線寫入的紀錄不會造成假的斷點
///
/// HMAC 的金鑰依節點的資料目錄取得（[`key_for`]）
pub fn verify(conn: &PgConn) -> PgResult<VerifyReport> {
    let key = key_for(conn);
    verify_with_key(conn, key.as_deref())
}

/// [`verify`]，指定 HMAC 的金鑰
pub fn verify_with_key(conn: &PgConn, key: Option<&AuditKey>) -> PgResult<VerifyReport> {
    if conn.in_transaction() {
        // 已在交易中：快照的範圍由呼叫端的交易決定
        return verify_in_snapshot(conn, key);
    }
    let tx = conn.begin_snapshot()?;
    let r = verify_in_snapshot(&tx, key)?;
    tx.commit()?;
    Ok(r)
}

fn verify_in_snapshot(conn: &PgConn, key: Option<&AuditKey>) -> PgResult<VerifyReport> {
    let anchor = anchor(conn)?;
    let head = meta_get(conn, META_HEAD)?.as_ref().and_then(Head::parse);
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
    let mut prev: Option<String> = None;
    // 逐列串流（不把整張表放進記憶體）；進行中這條連線不能送出其他敘述，所以需要的 meta 都已先讀出。
    // 回傳 `false`＝遇到斷點，停止
    conn.query_each(
        &format!("SELECT {COLUMNS} FROM audit_events ORDER BY id"),
        (),
        |r| {
            let row = row_of(r)?;
            report.checked += 1;
            let brk = |reason| ChainBreak {
                id: row.id,
                reason,
                at: Some(row.at),
            };
            let (Some(p), Some(h)) = (&row.prev_hash, &row.hash) else {
                report.broken = Some(brk("missing_hash"));
                return Ok(false);
            };
            match &prev {
                None => {
                    report.first_id = Some(row.id);
                    report.first_prev_hash = Some(p.clone());
                    // 清除過：剩下的第一筆接在錨點之後（全部清除後 id 可能重新使用，不以 id 判斷）
                    let expected = anchor.as_ref().map_or(GENESIS, |(_, h)| h.as_str());
                    if p != expected {
                        report.broken = Some(brk("anchor_mismatch"));
                        return Ok(false);
                    }
                }
                Some(pv) if pv != p => {
                    report.broken = Some(brk("prev_mismatch"));
                    return Ok(false);
                }
                _ => {}
            }
            if compute_hash(p, &row) != *h {
                report.broken = Some(brk("hash_mismatch"));
                return Ok(false);
            }
            // HMAC：邊界之前不應該有（有表示邊界被移動）；之後每一筆都必須有，金鑰可用時逐筆比對
            match &since {
                Some((sid, _)) if row.id >= *sid => match (&row.hmac, check_mac) {
                    (None, _) => {
                        report.broken = Some(brk("missing_hmac"));
                        return Ok(false);
                    }
                    (Some(m), Some(k)) => {
                        if !ct_eq_str(m, &k.record_mac(h)) {
                            report.broken = Some(brk("hmac_mismatch"));
                            return Ok(false);
                        }
                        report.hmac.verified += 1;
                    }
                    (Some(_), None) => {}
                },
                _ => {
                    if row.hmac.is_some() {
                        report.broken = Some(brk("hmac_boundary_mismatch"));
                        return Ok(false);
                    }
                    report.hmac.chain_only += 1;
                }
            }
            report.last_id = Some(row.id);
            report.last_hash = Some(h.clone());
            prev = Some(h.clone());
            Ok(true)
        },
    )?;
    // head：鏈尾與錨點（HMAC 開始之後必須存在、mac 正確；[`check_head`]）
    if report.broken.is_none() {
        let tail = report.last_id.zip(report.last_hash.as_deref());
        let sid = since.as_ref().map(|s| s.0);
        if let Err(reason) = check_head(head.as_ref(), tail, anchor.as_ref(), sid, check_mac) {
            report.broken = Some(ChainBreak {
                id: head
                    .as_ref()
                    .map(Head::tail_id)
                    .or(report.last_id)
                    .or(sid)
                    .unwrap_or(0),
                reason,
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
        && report.broken.as_ref().is_some_and(|b| {
            b.reason.contains("hmac") || b.reason == "head_mismatch" || b.at.is_none()
        })
    {
        report.hmac.status = "failed";
    }
    Ok(report)
}

/// 查詢條件的 WHERE 子句與參數（原本是 `AuditFilter::where_clause`）
fn where_clause(f: &AuditFilter) -> (String, Vec<super::conn::Value>) {
    use super::conn::Value as V;
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
            // 前綴比對以 strpos 實作（避免 LIKE 的萬用字元；區分大小寫，與 SQLite 的 instr 相同）
            push("strpos(action, ?::text) = 1", V::Text(a.clone()));
        } else {
            push("action = ?", V::Text(a.clone()));
        }
    }
    if let Some(t) = &f.target {
        push(
            "strpos(lower(coalesce(object, '')), ?::text) > 0",
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
pub fn query(conn: &PgConn, f: &AuditFilter, limit: usize) -> PgResult<(Vec<AuditRow>, u64)> {
    let (w, args) = where_clause(f);
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM audit_events{w} ORDER BY id DESC LIMIT {limit}"
    ))?;
    let rows = stmt
        .query_map(args, row_of)?
        .collect::<PgResult<Vec<_>>>()?;
    let total_filter = AuditFilter {
        before_id: None,
        ..f.clone()
    };
    let (w, args) = where_clause(&total_filter);
    let total: u64 = conn.query_row(
        &format!("SELECT count(*) FROM audit_events{w}"),
        args,
        |r| r.get::<_, u64>(0),
    )?;
    Ok((rows, total))
}

/// 匯出：舊到新，每批最多 `limit` 筆（以 `after_id` 接續）
pub fn export_batch(conn: &PgConn, f: &AuditFilter, limit: usize) -> PgResult<Vec<AuditRow>> {
    let (w, args) = where_clause(f);
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM audit_events{w} ORDER BY id LIMIT {limit}"
    ))?;
    stmt.query_map(args, row_of)?.collect()
}
