//! 稽核雜湊鏈的定期工作（0.7〔草稿〕；設計包 `decisions/INTEGRATION.md` §28.5 #10）：
//!
//! - **外送鏈尾**：每 `audit.anchor_log_minutes` 分鐘寫一行固定事件 `audit.chain_anchor`
//!   （[`CODE_ANCHOR`]，INFO），欄位 `last_id`、`last_hash`、`hmac_since`。以 syslog 或日誌收集器
//!   送到其他主機後，就是資料庫之外的副本：能改寫資料庫（甚至 HMAC 金鑰）的人也改不了已經送出的行。
//!   比對方式見 `kairos/API.md` 第 12.8 節。
//! - **自我驗證**：每 `audit.verify_interval_hours` 小時（第一次在啟動後 [`FIRST_RUN`]）重新計算整條鏈與
//!   HMAC。結果以 [`EventKind::AuditIntegrity`] 發布：失敗時告警引擎開「稽核日誌完整性」
//!   （`sys.audit_integrity`，緊急，`KRS-E6201`），下一次通過時恢復；失敗另記 ERROR
//!   （`audit.chain_broken`）。每次驗證寫一筆 `audit.verify`（執行者 `system`）。
//!
//! 驗證以唯讀連線進行（與 `POST /api/v1/audit/verify` 相同），不阻擋寫入。

use std::sync::Arc;
use std::time::Duration;

use crate::audit::{self, CODE_CHAIN_BROKEN};
use crate::bus::{EventBus, EventKind};
use crate::config::AuditSettings;
use crate::enroll::db::{DbError, EnrollDb};
use crate::shutdown::Shutdown;

/// 定期外送的鏈尾雜湊（子類別 62「稽核日誌」；設計包 `decisions/INTEGRATION.md` §29.3 正式分配）
pub const CODE_ANCHOR: &str = "KRS-I6202";
/// 第一次外送與驗證：啟動後多久（啟動時的寫入先完成）
pub const FIRST_RUN: Duration = Duration::from_secs(60);

/// 鏈尾：(id, hash, hmac)
type Tail = (i64, Option<String>, Option<String>);

/// 鏈尾；沒有紀錄時為 `None`
fn tail(conn: &rusqlite::Connection) -> rusqlite::Result<Option<Tail>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT id, hash, hmac FROM audit_events ORDER BY id DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

/// 寫一行鏈尾的固定事件
pub async fn log_anchor(db: &Arc<EnrollDb>) {
    let r = db
        .read(|conn| Ok((tail(conn)?, audit::hmac_since(conn)?)))
        .await;
    match r {
        Ok((Some((id, hash, mac)), since)) => {
            let hash = hash.unwrap_or_default();
            let since_id = since.map(|s| s.0);
            log::info!(
                event = "audit.chain_anchor", code = CODE_ANCHOR,
                last_id = id, last_hash = hash.as_str(),
                last_hmac = mac.as_deref().unwrap_or(""),
                hmac_since = since_id.unwrap_or(0);
                "資訊 {CODE_ANCHOR}：稽核鏈尾 #{id} {hash}（HMAC {}）",
                since_id.map_or_else(|| "尚未開始".to_string(), |s| format!("自 #{s}"))
            );
        }
        Ok((None, _)) => {
            log::info!(
                event = "audit.chain_anchor", code = CODE_ANCHOR, last_id = 0;
                "資訊 {CODE_ANCHOR}：稽核日誌目前沒有紀錄"
            );
        }
        Err(e) => log::warn!(event = "audit.anchor_failed"; "無法讀取稽核鏈尾：{e}"),
    }
}

/// 驗證一次並發布結果；回傳是否完整（無法讀取資料庫時為 `None`，不發布）
pub async fn self_verify(db: &Arc<EnrollDb>, bus: &EventBus) -> Option<bool> {
    // 升級前（沒有雜湊）的紀錄先補上（只有第一次），再以唯讀連線驗證
    if let Err(e) = db.write(|c| Ok(audit::seal_legacy(c)?)).await {
        log::warn!(event = "audit.verify_failed"; "稽核日誌的自我驗證無法執行：{e}");
        return None;
    }
    let report = match db.read(|c| Ok(audit::verify(c)?)).await {
        Ok(r) => r,
        Err(DbError(e)) => {
            log::warn!(event = "audit.verify_failed"; "稽核日誌的自我驗證無法執行：{e}");
            return None;
        }
    };
    let ok = report.ok();
    let summary = match &report.broken {
        None => format!(
            "已驗證 {} 筆（HMAC：{}）",
            report.checked, report.hmac.status
        ),
        Some(b) => format!("第 {} 筆：{}", b.id, describe(b.reason)),
    };
    if let Some(b) = &report.broken {
        log::error!(
            event = "audit.chain_broken", code = CODE_CHAIN_BROKEN;
            "錯誤 {CODE_CHAIN_BROKEN}：稽核日誌的定期自我驗證失敗：第 {} 筆（{}）。紀錄可能被修改或刪除，請保全資料庫檔並與匯出或外送的鏈尾（audit.chain_anchor）比對",
            b.id, b.reason
        );
    } else {
        log::info!(
            event = "audit.verified";
            "稽核日誌的定期自我驗證通過：{summary}"
        );
    }
    bus.publish(EventKind::AuditIntegrity {
        ok,
        reason: report.broken.as_ref().map(|b| b.reason.to_string()),
        id: report.broken.as_ref().map(|b| b.id),
        summary: summary.clone(),
    });
    let details = serde_json::json!({
        "via": "self_check",
        "checked": report.checked,
        "hmac": report.hmac.status,
        "hmac_since_id": report.hmac.since_id,
        "broken": report.broken.as_ref().map(|b| serde_json::json!({"id": b.id, "reason": b.reason})),
    });
    let now = chrono::Utc::now().timestamp_millis();
    let r = db
        .write(move |conn| {
            audit::append(
                conn,
                now,
                &audit::Audit {
                    actor: "system",
                    category: "audit",
                    action: "audit.verify",
                    result: if ok { "ok" } else { "failed" },
                    code: (!ok).then_some(CODE_CHAIN_BROKEN),
                    details: Some(details),
                    ..Default::default()
                },
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = r {
        log::warn!(event = "audit.write_failed"; "無法寫入稽核紀錄（audit.verify）：{e}");
    }
    Some(ok)
}

/// 斷點原因的說明（CLI、日誌、告警事件共用）
pub fn describe(reason: &str) -> &'static str {
    match reason {
        "hash_mismatch" => "內容與雜湊不符",
        "prev_mismatch" => "與前一筆接不上",
        "missing_hash" => "沒有雜湊",
        "anchor_mismatch" => "第一筆與清除時的錨點不符",
        "head_mismatch" => "最後幾筆被刪除或最新一筆被修改",
        "missing_hmac" => "HMAC 起點之後的紀錄沒有 HMAC",
        "hmac_mismatch" => "HMAC 不符（紀錄或雜湊被重新計算）",
        "hmac_boundary_mismatch" => "HMAC 的起點被移動",
        "hmac_key_unavailable" => "HMAC 的金鑰檔（audit-hmac.key）遺失或無法讀取",
        "hmac_key_mismatch" => "HMAC 的金鑰檔與起點記錄的金鑰不符",
        _ => "雜湊鏈斷裂",
    }
}

/// 定期工作（直到關機）
pub async fn run(
    db: Arc<EnrollDb>,
    bus: Arc<EventBus>,
    settings: AuditSettings,
    shutdown: Arc<Shutdown>,
) {
    let anchor_every = (settings.anchor_log_minutes > 0)
        .then(|| Duration::from_secs(u64::from(settings.anchor_log_minutes) * 60));
    let verify_every = Duration::from_secs(u64::from(settings.verify_interval_hours.max(1)) * 3600);
    let start = tokio::time::Instant::now();
    let mut next_anchor = anchor_every.map(|_| start + FIRST_RUN);
    let mut next_verify = start + FIRST_RUN;
    loop {
        let wake = next_anchor.map_or(next_verify, |a| a.min(next_verify));
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep_until(wake) => {}
        }
        let now = tokio::time::Instant::now();
        if let (Some(at), Some(every)) = (next_anchor, anchor_every)
            && now >= at
        {
            log_anchor(&db).await;
            next_anchor = Some(now + every);
        }
        if now >= next_verify {
            self_verify(&db, &bus).await;
            next_verify = now + verify_every;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (std::path::PathBuf, Arc<EnrollDb>) {
        let dir = std::env::temp_dir().join(format!(
            "kairos-audit-monitor-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(crate::store::sqlite::DB_FILE);
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        crate::store::sqlite::migrate(&mut conn, crate::store::sqlite::MIGRATIONS).unwrap();
        drop(conn);
        (dir, EnrollDb::new(path))
    }

    fn ev(action: &'static str) -> audit::Audit<'static> {
        audit::Audit {
            actor: "user:admin",
            category: "user",
            action,
            result: "ok",
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn self_verification_publishes_the_result_and_tampering_opens_the_condition() {
        let (dir, db) = temp_db();
        audit::ensure_key(&dir).unwrap();
        db.write(|c| {
            for i in 0..5 {
                audit::append(c, i, &ev("user.create"))?;
            }
            Ok(())
        })
        .await
        .unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        assert_eq!(self_verify(&db, &bus).await, Some(true));
        match &rx.recv().await.unwrap().kind {
            EventKind::AuditIntegrity { ok, reason, .. } => {
                assert!(ok);
                assert_eq!(reason, &None);
            }
            other => panic!("{other:?}"),
        }
        // 繞過程式修改一筆並重新計算之後的雜湊鏈（沒有 HMAC 金鑰）：HMAC 不符
        db.write(|c| {
            c.execute("UPDATE audit_events SET object = 'eve' WHERE id = 2", [])?;
            let rows: Vec<audit::AuditRow> =
                audit::export_batch(c, &audit::AuditFilter::default(), 100)?;
            let mut prev = audit::GENESIS.to_string();
            for r in rows {
                let h = audit::compute_hash(&prev, &r);
                c.execute(
                    "UPDATE audit_events SET prev_hash = ?2, hash = ?3 WHERE id = ?1",
                    rusqlite::params![r.id, prev, h],
                )?;
                prev = h;
            }
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(self_verify(&db, &bus).await, Some(false));
        match &rx.recv().await.unwrap().kind {
            EventKind::AuditIntegrity { ok, reason, id, .. } => {
                assert!(!ok);
                assert_eq!(reason.as_deref(), Some("hmac_mismatch"));
                assert_eq!(*id, Some(2));
            }
            other => panic!("{other:?}"),
        }
        // 每次自我驗證都寫入稽核（執行者 system）
        let n: i64 = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM audit_events WHERE action = 'audit.verify' AND actor = 'system'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(n, 2);
        log_anchor(&db).await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_break_reason_has_a_description() {
        for r in [
            "hash_mismatch",
            "prev_mismatch",
            "missing_hash",
            "anchor_mismatch",
            "head_mismatch",
            "missing_hmac",
            "hmac_mismatch",
            "hmac_boundary_mismatch",
            "hmac_key_unavailable",
            "hmac_key_mismatch",
        ] {
            assert_ne!(describe(r), "雜湊鏈斷裂", "{r}");
        }
    }
}
