//! 稽核事件的雜湊鏈（設計包 `kairos/screens.md` S18、§24 #11；審計 F6、S7）。
//!
//! 每一筆 `audit_events` 有 `prev_hash`（前一筆的 `hash`）與 `hash`：
//!
//! ```text
//! hash = SHA-256( prev_hash ‖ "\n" ‖ JSON([id, at, actor, actor_ip, category, action, object, result, code, details]) )
//! ```
//!
//! - JSON 是 serde_json 的精簡格式（沒有空白；非 ASCII 字元原樣以 UTF-8 輸出）；`details` 是資料庫中保存的
//!   字串本身（不是解析後的物件），沒有值時為 `null`。雜湊以小寫十六進位保存。
//! - 第一筆的 `prev_hash` 是 64 個 `0`（[`GENESIS`]）。保留期限清除最舊的紀錄時，被清除的最後一筆的
//!   `hash` 記在 `meta.audit_chain_anchor`，剩下的第一筆的 `prev_hash` 必須等於它。
//! - 最新一筆的 id 與 hash 另記在 `meta.audit_chain_head`：只刪除最後幾筆的竄改也會被發現（整條鏈被重新
//!   計算時無法由資料庫本身發現，需要外送或匯出的副本比對，見 `kairos/API.md` 第 12 節）。
//! - 寫入一律經由 [`append`]：先 INSERT（取得寫入鎖）再計算雜湊，同一個寫入交易中不會有其他行程插入，
//!   CLI 與 Kairos 同時寫入也不會讓鏈分岔。不在交易中呼叫時自己開 `BEGIN IMMEDIATE`。
//! - schema v6 之前寫入的紀錄（沒有雜湊）在升級後**第一次**寫入或驗證時依 id 順序補上，之後在
//!   `meta.audit_legacy_sealed` 記下已補上（[`seal_legacy`]）。之後只有 [`append`] 本身寫入的那一筆會被計算
//!   雜湊：繞過程式直接插入資料庫的紀錄（沒有雜湊）不會被「合法化」，驗證時以 `missing_hash` 指出。
//! - 任何角色都不能經由 API 或 CLI 修改或刪除紀錄（只有保留期限的清除）。

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// 第一筆的 `prev_hash`
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// `meta` 的鍵：被保留期限清除的最後一筆（`{"id","hash","purged_at","purged"}`）
pub const META_ANCHOR: &str = "audit_chain_anchor";
/// `meta` 的鍵：最新一筆（`{"id","hash"}`）
pub const META_HEAD: &str = "audit_chain_head";
/// `meta` 的鍵：升級前（沒有雜湊）的紀錄已補上雜湊（`{"through_id","sealed_at"}`）
pub const META_LEGACY_SEALED: &str = "audit_legacy_sealed";
/// 完整性驗證失敗的產品代碼（`copywriting.md` §3.3）
pub const CODE_CHAIN_BROKEN: &str = "KRS-E6201";

/// 一筆稽核事件（寫入用）
#[derive(Debug, Clone, Default)]
pub struct Audit<'a> {
    /// 執行者：`user:<帳號>`、`token:<token_id>`、作業系統帳號（CLI）、`system`、`anonymous`
    pub actor: &'a str,
    pub actor_ip: Option<&'a str>,
    /// 類別：`login`、`user`、`api_token`、`settings`、`enroll`、`pki`、`host`、`audit`、`system`
    pub category: &'a str,
    /// 動作（`<對象>.<動詞>`，例如 `login.success`、`user.disable`、`token.create`）
    pub action: &'a str,
    /// 對象（帳號、token_id、序號、主機、設定鍵…）
    pub object: Option<&'a str>,
    /// 結果：`ok`（成功）、`failed`（失敗）、`denied`（拒絕；權限不足、帳號鎖定）、`rejected`
    pub result: &'a str,
    /// 產品代碼（`KRS-…`）
    pub code: Option<&'a str>,
    /// 其他欄位（原因、變更前後的差異…；不含密碼與 token）
    pub details: Option<Value>,
}

/// [`Audit`] 的擁有所有權版本（跨 `await`、送進 blocking 執行緒時使用）
#[derive(Debug, Clone, Default)]
pub struct Event {
    pub actor: String,
    pub actor_ip: Option<String>,
    pub category: &'static str,
    pub action: &'static str,
    pub object: Option<String>,
    pub result: &'static str,
    pub code: Option<&'static str>,
    pub details: Option<Value>,
}

impl Event {
    pub fn append(&self, conn: &Connection, at_ms: i64) -> rusqlite::Result<i64> {
        append(
            conn,
            at_ms,
            &Audit {
                actor: &self.actor,
                actor_ip: self.actor_ip.as_deref(),
                category: self.category,
                action: self.action,
                object: self.object.as_deref(),
                result: self.result,
                code: self.code,
                details: self.details.clone(),
            },
        )
    }
}

/// 資料庫中的一筆
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub actor_ip: Option<String>,
    pub category: String,
    pub action: String,
    pub object: Option<String>,
    pub result: String,
    pub code: Option<String>,
    pub details: Option<String>,
    pub prev_hash: Option<String>,
    pub hash: Option<String>,
}

const COLUMNS: &str =
    "id, at, actor, actor_ip, category, action, object, result, code, details, prev_hash, hash";

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
    })
}

/// 一筆紀錄的雜湊（模組說明的算式）
pub fn compute_hash(prev_hash: &str, r: &AuditRow) -> String {
    let record = json!([
        r.id, r.at, r.actor, r.actor_ip, r.category, r.action, r.object, r.result, r.code,
        r.details
    ]);
    let mut input = Vec::with_capacity(256);
    input.extend_from_slice(prev_hash.as_bytes());
    input.push(b'\n');
    input.extend_from_slice(record.to_string().as_bytes());
    hex(ring::digest::digest(&ring::digest::SHA256, &input).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

/// 寫入一筆稽核事件並接上雜湊鏈；回傳 id
pub fn append(conn: &Connection, at_ms: i64, a: &Audit<'_>) -> rusqlite::Result<i64> {
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
        Ok(id)
    })
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

/// 超過上限的字串截短（以字元為單位；避免以超長的帳號名稱塞滿紀錄）
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
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

/// 驗證結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// 檢查的筆數
    pub checked: u64,
    pub first_id: Option<i64>,
    pub last_id: Option<i64>,
    /// 最後一筆的雜湊（鏈的終點）
    pub last_hash: Option<String>,
    /// 鏈的起點（第一筆的 `prev_hash`）
    pub first_prev_hash: Option<String>,
    /// 第一個斷點
    pub broken: Option<ChainBreak>,
}

/// 斷點
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainBreak {
    /// 斷在哪一筆（被刪除的最後幾筆時為 head 記錄的 id）
    pub id: i64,
    /// `hash_mismatch`（內容被修改）、`prev_mismatch`（前一筆被刪除或修改、順序被改變）、
    /// `missing_hash`（沒有雜湊，例如被人工插入）、`anchor_mismatch`（第一筆與清除時的錨點不符）、
    /// `head_mismatch`（最後幾筆被刪除、或最新一筆被修改）
    pub reason: &'static str,
    pub at: Option<i64>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.broken.is_none()
    }

    pub fn json(&self) -> Value {
        json!({
            "ok": self.ok(),
            "checked": self.checked,
            "first_id": self.first_id,
            "last_id": self.last_id,
            "first_prev_hash": self.first_prev_hash,
            "last_hash": self.last_hash,
            "broken": self.broken.as_ref().map(|b| json!({
                "id": b.id,
                "reason": b.reason,
                "at": b.at.map(crate::api::http::ts_ms),
                "product_code": CODE_CHAIN_BROKEN,
            })),
        })
    }
}

/// 從頭到尾重新計算雜湊鏈，回報第一個斷點（不修改任何資料）。所有讀取在同一個讀取交易（同一個
/// 快照）中進行：驗證期間其他連線寫入的紀錄不會造成假的斷點
pub fn verify(conn: &Connection) -> rusqlite::Result<VerifyReport> {
    if !conn.is_autocommit() {
        return verify_in_snapshot(conn);
    }
    conn.execute_batch("BEGIN")?;
    let r = verify_in_snapshot(conn);
    let _ = conn.execute_batch(if r.is_ok() { "COMMIT" } else { "ROLLBACK" });
    r
}

fn verify_in_snapshot(conn: &Connection) -> rusqlite::Result<VerifyReport> {
    let anchor = anchor(conn)?;
    let head = meta_get(conn, META_HEAD)?
        .and_then(|v| Some((v.get("id")?.as_i64()?, v.get("hash")?.as_str()?.to_string())));
    let mut report = VerifyReport {
        checked: 0,
        first_id: None,
        last_id: None,
        last_hash: None,
        first_prev_hash: None,
        broken: None,
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
        report.last_id = Some(row.id);
        report.last_hash = Some(h.clone());
        prev = Some(h.clone());
    }
    if report.broken.is_none()
        && let Some((hid, hh)) = head
    {
        let ok = report.last_id == Some(hid) && report.last_hash.as_deref() == Some(hh.as_str());
        if !ok {
            report.broken = Some(ChainBreak {
                id: hid,
                reason: "head_mismatch",
                at: None,
            });
        }
    }
    Ok(report)
}

/// 查詢條件（S18 的 FilterBar）
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// 執行者（完全相同）
    pub actor: Option<String>,
    pub category: Option<String>,
    /// 動作（完全相同，或以 `.` 結尾時為前綴：`user.`）
    pub action: Option<String>,
    /// 對象（片段，不分大小寫）
    pub target: Option<String>,
    pub result: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// 游標：只列出 id 小於它的（新到舊）
    pub before_id: Option<i64>,
    /// 匯出：只列出 id 大於它的（舊到新）
    pub after_id: Option<i64>,
}

impl AuditFilter {
    fn where_clause(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value as V;
        let mut conds = Vec::new();
        let mut args: Vec<V> = Vec::new();
        let mut push = |cond: &str, v: V| {
            args.push(v);
            conds.push(cond.replace('?', &format!("?{}", args.len())));
        };
        if let Some(a) = &self.actor {
            push("actor = ?", V::Text(a.clone()));
        }
        if let Some(c) = &self.category {
            push("category = ?", V::Text(c.clone()));
        }
        if let Some(a) = &self.action {
            if a.ends_with('.') {
                // 前綴比對以 instr 實作（避免 LIKE 的萬用字元）
                push("instr(action, ?) = 1", V::Text(a.clone()));
            } else {
                push("action = ?", V::Text(a.clone()));
            }
        }
        if let Some(t) = &self.target {
            push(
                "instr(lower(coalesce(object, '')), ?) > 0",
                V::Text(t.to_lowercase()),
            );
        }
        if let Some(r) = &self.result {
            push("result = ?", V::Text(r.clone()));
        }
        if let Some(f) = self.from_ms {
            push("at >= ?", V::Integer(f));
        }
        if let Some(t) = self.to_ms {
            push("at < ?", V::Integer(t));
        }
        if let Some(b) = self.before_id {
            push("id < ?", V::Integer(b));
        }
        if let Some(a) = self.after_id {
            push("id > ?", V::Integer(a));
        }
        let w = if conds.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conds.join(" AND "))
        };
        (w, args)
    }
}

/// 新到舊的 `limit` 筆，以及符合條件的總數
pub fn query(
    conn: &Connection,
    f: &AuditFilter,
    limit: usize,
) -> rusqlite::Result<(Vec<AuditRow>, u64)> {
    let (w, args) = f.where_clause();
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
    let (w, args) = total_filter.where_clause();
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
    let (w, args) = f.where_clause();
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM audit_events{w} ORDER BY id LIMIT {limit}"
    ))?;
    stmt.query_map(rusqlite::params_from_iter(args.iter()), row_of)?
        .collect()
}

impl AuditRow {
    /// API 與匯出的 JSON（`details` 解析成物件；時間為 RFC 3339 UTC）
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "at": crate::api::http::ts_ms(self.at),
            "actor": self.actor,
            "actor_ip": self.actor_ip,
            "category": self.category,
            "action": self.action,
            "object": self.object,
            "result": self.result,
            "code": self.code,
            "details": self.details.as_deref().and_then(|d| serde_json::from_str::<Value>(d).ok()),
            "prev_hash": self.prev_hash,
            "hash": self.hash,
        })
    }

    /// 匯出的一行：[`AuditRow::json`] 另帶 `at_ms` 與 `details_raw`（重新計算雜湊所需的原始值）
    pub fn export_json(&self) -> Value {
        let mut v = self.json();
        v["type"] = json!("event");
        v["at_ms"] = json!(self.at);
        v["details_raw"] = json!(self.details);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
