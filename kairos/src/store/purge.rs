//! 刪除一台主機的資料（`kairos hosts purge`、`POST /api/v1/hosts/{agent_id}/purge`；商業審計 S6，
//! `kairos/PRIVACY.md` 第 4 節）。
//!
//! 以 `agent_id` 刪除這台主機在資料庫中的所有資料：
//!
//! | 資料 | 資料表 |
//! |---|---|
//! | 原始 report、行程快照 | `reports`、`process_snapshots` |
//! | 彙總 | `rollup_5m`、`rollup_1h` |
//! | 資產與變更歷史 | `host_inventory`、`inventory_changes` |
//! | 序列、去重鍵 | `host_series`、`boots`、`import_keys` |
//! | 告警事件（含處理紀錄、發送紀錄、CLI 動作） | `alert_events`、`alert_timeline`、`alert_deliveries`、`alert_actions` |
//! | 註冊申請、續簽的重放快取 | `enroll_requests`、`enroll_replay` |
//! | 憑證的簽發紀錄 | `pki_certificates`（**已撤銷且尚未過期**的保留到過期後 1 天：CRL 需要它們，刪掉會讓撤銷失效；尚未撤銷的有效憑證由呼叫端先撤銷，[`revoke_active_certificates`]） |
//! | 主機清冊 | `hosts`（最後一步） |
//!
//! **稽核日誌不刪除**：`audit_events` 是雜湊鏈（每一筆的雜湊涵蓋前一筆），刪除或修改任何一筆都會讓
//! 完整性驗證失敗；稽核紀錄依 `audit.retention_days` 到期刪除。刪除本身寫一筆 `host.purge`（只有
//! `agent_id` 與各資料表的筆數，不含主機名稱與位址）。
//!
//! 大量的資料表（report、行程快照、彙總、資產變更）每一步刪除最多 `budget` 列（一個交易），其餘在最後
//! 一步的同一個交易中刪除，`hosts` 與稽核紀錄也在這個交易中：中途失敗或中斷時清冊仍在，重新執行即可完成。
//! 執行中的 Kairos 由 writer 逐步執行（[`crate::store::sqlite::SqliteStore`]，與 report 的批次交錯），
//! `kairos hosts purge` 在 Kairos 停止時以獨占鎖執行。

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Value, json};

/// 憑證過期之後仍在 CRL 中的時間（與 PKI.md 第 10 節「CRL 收錄 `not_after` 尚未超過 1 天以上的撤銷」相同）
pub const CRL_KEEP_AFTER_EXPIRY_MS: i64 = 86_400_000;

/// 各資料表刪除（或會刪除）的列數
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PurgeCounts {
    pub reports: u64,
    pub process_snapshots: u64,
    pub rollup_5m: u64,
    pub rollup_1h: u64,
    /// 記憶體中尚未寫出的彙總列（只有執行中的 Kairos 有）
    pub rollup_pending: u64,
    pub inventory: u64,
    pub inventory_changes: u64,
    pub host_series: u64,
    pub boots: u64,
    pub import_keys: u64,
    pub alert_events: u64,
    pub alert_timeline: u64,
    pub alert_deliveries: u64,
    pub enroll_requests: u64,
    pub certificates: u64,
    /// 已撤銷、尚未過期而保留的憑證紀錄（CRL 需要；過期 1 天後由每小時的清除刪除）
    pub certificates_kept: u64,
    /// 尚未撤銷、尚未過期的憑證（刪除前先以 `cessationOfOperation` 撤銷；試執行時的預估）
    pub certificates_active: u64,
    pub hosts: u64,
}

impl PurgeCounts {
    /// 會刪除的列數合計（不含保留的憑證紀錄）
    pub fn total(&self) -> u64 {
        self.reports
            + self.process_snapshots
            + self.rollup_5m
            + self.rollup_1h
            + self.inventory
            + self.inventory_changes
            + self.host_series
            + self.boots
            + self.import_keys
            + self.alert_events
            + self.alert_timeline
            + self.alert_deliveries
            + self.enroll_requests
            + self.certificates
            + self.hosts
    }

    pub fn json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(m) = &mut v {
            m.insert("total".into(), json!(self.total()));
        }
        v
    }

    /// 人看的清單：(說明, 筆數)
    pub fn rows(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("原始 report", self.reports),
            ("行程快照", self.process_snapshots),
            ("5 分鐘彙總", self.rollup_5m),
            ("1 小時彙總", self.rollup_1h),
            ("資產", self.inventory),
            ("資產變更歷史", self.inventory_changes),
            ("告警事件", self.alert_events),
            ("告警處理紀錄", self.alert_timeline),
            ("通知發送紀錄", self.alert_deliveries),
            ("註冊申請", self.enroll_requests),
            ("憑證紀錄", self.certificates),
            ("主機清冊", self.hosts),
        ]
    }
}

/// 主機編號（清冊中沒有時為 `None`：例如只有註冊申請、尚未簽發）
pub fn host_id_of(conn: &Connection, agent_id: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT host_id FROM hosts WHERE agent_id = ?1",
        [agent_id],
        |r| r.get(0),
    )
    .optional()
}

fn count(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<u64> {
    conn.query_row(sql, p, |r| r.get::<_, i64>(0))
        .map(|n| n.max(0) as u64)
}

/// 已撤銷、撤銷已生效（或排定）、且還在 CRL 期間的憑證：不能刪
const CERT_KEPT: &str = "revoked_at IS NOT NULL AND not_after + ?2 > ?3";
/// 尚未撤銷、尚未過期
const CERT_ACTIVE: &str = "revoked_at IS NULL AND not_after > ?3";

/// 試執行與影響預覽：各資料表會刪除的列數。`agent_id` 在任何資料表都沒有資料時全部為 0
pub fn plan(conn: &Connection, agent_id: &str, now_ms: i64) -> rusqlite::Result<PurgeCounts> {
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
    }
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
    let all = count(
        conn,
        "SELECT COUNT(*) FROM pki_certificates WHERE agent_id = ?1 AND ?2 = ?2 AND ?3 = ?3",
        p,
    )?;
    c.certificates_active = count(
        conn,
        &format!(
            "SELECT COUNT(*) FROM pki_certificates WHERE agent_id = ?1 AND ?2 = ?2 AND {CERT_ACTIVE}"
        ),
        p,
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

/// 進行中的步驟
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    #[default]
    Reports,
    Processes,
    Rollup5m,
    Rollup1h,
    Changes,
    Finish,
    Done,
}

/// 一次刪除工作
#[derive(Debug, Clone, Default)]
pub struct PurgeJob {
    pub agent_id: String,
    /// 執行者（`user:<帳號>`、`token:<id>`、作業系統帳號）與來源 IP
    pub actor: String,
    pub actor_ip: Option<String>,
    pub reason: Option<String>,
    /// 刪除前撤銷的憑證序號（稽核紀錄）
    pub revoked: Vec<String>,
    /// 第一步時查出（writer 另外清除快取）
    pub host_id: Option<i64>,
    pub started: bool,
    pub stage: Stage,
    pub counts: PurgeCounts,
}

impl PurgeJob {
    pub fn new(agent_id: &str, actor: &str, actor_ip: Option<&str>, reason: Option<&str>) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            actor: actor.to_string(),
            actor_ip: actor_ip.map(str::to_string),
            reason: reason.map(str::to_string),
            ..Self::default()
        }
    }
}

/// 做一步（一個交易）。`budget`：大量資料表每一步最多刪除的列數。回傳是否已完成
pub fn step(
    conn: &mut Connection,
    job: &mut PurgeJob,
    budget: i64,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    if !job.started {
        job.host_id = host_id_of(conn, &job.agent_id)?;
        job.started = true;
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
    }
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
        p,
    )? as u64;
    c.certificates_kept = count(
        &tx,
        "SELECT COUNT(*) FROM pki_certificates WHERE agent_id = ?1 AND ?2 = ?2 AND ?3 = ?3",
        p,
    )?;
    c.certificates_active = count(
        &tx,
        &format!(
            "SELECT COUNT(*) FROM pki_certificates WHERE agent_id = ?1 AND ?2 = ?2 AND {CERT_ACTIVE}"
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
    crate::audit::append(
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

/// 已刪除主機資料（日誌與稽核；〔暫定〕代碼，待設計包分配）
pub const CODE_PURGED: &str = "KRS-I4204";
/// 無法刪除主機資料（主機仍連線中、找不到資料、刪除失敗；〔暫定〕）
pub const CODE_PURGE_FAILED: &str = "KRS-E4205";

/// 一次做完（`kairos hosts purge`：Kairos 停止、已取得獨占鎖）
pub fn run_to_end(conn: &mut Connection, job: &mut PurgeJob, now_ms: i64) -> rusqlite::Result<()> {
    while !step(conn, job, 20_000, now_ms)? {}
    Ok(())
}

/// 已撤銷而保留的憑證紀錄：主機資料已刪除（清冊中沒有）、過期超過 1 天之後刪除（註冊服務每小時的清除）
pub fn delete_orphan_certificates(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM pki_certificates WHERE not_after + ?1 <= ?2
         AND agent_id NOT IN (SELECT agent_id FROM hosts)",
        params![CRL_KEEP_AFTER_EXPIRY_MS, now_ms],
    )
}

/// 刪除前撤銷這台主機尚未撤銷、尚未過期的憑證（`cessationOfOperation`，立即生效；每張一筆稽核
/// `cert.revoke`）。回傳撤銷的序號。執行中的 Kairos 由註冊服務在 2 秒內（主控台會立即喚醒）重新產生 CRL；
/// 由外部 CA 簽發、匯入的憑證另以 `kairos pki revocations export` 交給 CA 產生 CRL
pub fn revoke_active_certificates(
    conn: &Connection,
    agent_id: &str,
    actor: &str,
    actor_ip: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<Vec<String>> {
    use crate::pki::issue::Reason;
    use crate::pki::store::{self as ps, Revoke};
    let serials: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT serial FROM pki_certificates
             WHERE agent_id = ?1 AND revoked_at IS NULL AND not_after > ?2 ORDER BY issued_at",
        )?;
        let rows = stmt.query_map(params![agent_id, now_ms], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if serials.is_empty() {
        return Ok(serials);
    }
    let tx = conn.unchecked_transaction()?;
    let mut out = Vec::new();
    for s in serials {
        if let Revoke::Now(_) = ps::revoke(
            &tx,
            &s,
            now_ms,
            now_ms,
            Reason::CessationOfOperation,
            actor,
            Some("刪除主機資料"),
        )? {
            crate::audit::append(
                &tx,
                now_ms,
                &crate::audit::Audit {
                    actor,
                    actor_ip,
                    category: "pki",
                    action: "cert.revoke",
                    object: Some(&s),
                    result: "ok",
                    details: Some(json!({
                        "reason": "cessationOfOperation",
                        "comment": "刪除主機資料",
                        "agent_id": agent_id,
                    })),
                    ..crate::audit::Audit::default()
                },
            )?;
            out.push(s);
        }
    }
    tx.commit()?;
    Ok(out)
}

/// 影響預覽的確認 token：主機與會撤銷的憑證不變時相同（5 分鐘的時間窗，前一個時間窗也接受；
/// 與 `kairos/API.md` 第 11.4 節的 `impact_token` 相同的作法）
pub fn impact_token(agent_id: &str, c: &PurgeCounts, now_ms: i64) -> String {
    let w = now_ms / crate::enroll::ops::IMPACT_WINDOW_MS;
    token_at(agent_id, c, w)
}

fn token_at(agent_id: &str, c: &PurgeCounts, window: i64) -> String {
    let text = format!(
        "purge|{agent_id}|{}|{}|{}|{}|{window}",
        c.hosts, c.certificates_active, c.alert_events, c.enroll_requests
    );
    kairos_proto::enroll::sha256_hex(text.as_bytes())[..24].to_string()
}

/// `impact_token` 是否仍符合目前的數字
pub fn verify_impact_token(agent_id: &str, c: &PurgeCounts, token: &str, now_ms: i64) -> bool {
    let w = now_ms / crate::enroll::ops::IMPACT_WINDOW_MS;
    token == token_at(agent_id, c, w) || token == token_at(agent_id, c, w - 1)
}

#[cfg(test)]
#[path = "purge_tests.rs"]
mod tests;
