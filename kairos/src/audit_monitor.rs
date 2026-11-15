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
/// 多久看一次有沒有新的重新錨定（`meta.audit_reanchor`）
pub const REANCHOR_POLL: Duration = Duration::from_secs(60);

/// 寫一行鏈尾的固定事件
pub async fn log_anchor(db: &Arc<EnrollDb>) {
    let r = db
        .read(|conn| Ok((conn.audit_tail()?, conn.audit_hmac_since()?)))
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
    if let Err(e) = db.write(|c| Ok(c.audit_seal_legacy()?)).await {
        log::warn!(event = "audit.verify_failed"; "稽核日誌的自我驗證無法執行：{e}");
        return None;
    }
    let report = match db.read(|c| Ok(c.audit_verify()?)).await {
        Ok(r) => r,
        Err(DbError(e)) => {
            log::warn!(event = "audit.verify_failed"; "稽核日誌的自我驗證無法執行：{e}");
            return None;
        }
    };
    let chain_ok = report.ok();
    // 最近重新錨定過（`kairos audit reanchor`）：鏈從新的起點起算是完整的，但「稽核日誌完整性」告警保持
    // 開啟 [`audit::REANCHOR_ALERT_HOLD_MS`]——斷鏈之後重新錨定不可能不被看到（告警不可靜音）
    let now_ms = chrono::Utc::now().timestamp_millis();
    let held = report
        .reanchor
        .as_ref()
        .filter(|r| chain_ok && now_ms - r.at < audit::REANCHOR_ALERT_HOLD_MS);
    let ok = chain_ok && held.is_none();
    let summary = match (&report.broken, held) {
        (Some(b), _) => format!("第 {} 筆：{}", b.id, describe(b.reason)),
        (None, Some(r)) => format!(
            "稽核鏈已由 {} 在 {} 重新錨定（第 {} 筆）：之前的 {} 筆完整性未驗證",
            r.by,
            crate::api::http::ts_ms(r.at),
            r.event_id,
            r.unverified
        ),
        (None, None) => format!(
            "已驗證 {} 筆（HMAC：{}）",
            report.checked, report.hmac.status
        ),
    };
    if let Some(r) = held {
        log::error!(
            event = "audit.reanchored", code = CODE_CHAIN_BROKEN, reanchor_id = r.event_id;
            "錯誤 {CODE_CHAIN_BROKEN}：{summary}。這是管理者以 kairos audit reanchor 執行的；如果不是預期中的操作，請立即保全資料庫並以外送的鏈尾（audit.chain_anchor）比對"
        );
    }
    if let Some(b) = &report.broken {
        log::error!(
            event = "audit.chain_broken", code = CODE_CHAIN_BROKEN;
            "錯誤 {CODE_CHAIN_BROKEN}：稽核日誌的定期自我驗證失敗：第 {} 筆（{}）。紀錄可能被修改或刪除，請保全資料庫檔並與匯出或外送的鏈尾（audit.chain_anchor）比對",
            b.id, b.reason
        );
    } else if held.is_none() {
        log::info!(
            event = "audit.verified";
            "稽核日誌的定期自我驗證通過：{summary}"
        );
    }
    bus.publish(EventKind::AuditIntegrity {
        ok,
        reason: report
            .broken
            .as_ref()
            .map(|b| b.reason.to_string())
            .or(held.map(|_| "reanchored".to_string())),
        id: report
            .broken
            .as_ref()
            .map(|b| b.id)
            .or(held.map(|r| r.event_id)),
        summary: summary.clone(),
    });
    let details = serde_json::json!({
        "via": "self_check",
        "checked": report.checked,
        "hmac": report.hmac.status,
        "hmac_since_id": report.hmac.since_id,
        "broken": report.broken.as_ref().map(|b| serde_json::json!({"id": b.id, "reason": b.reason})),
        "reanchor": report.reanchor.as_ref().map(|r| serde_json::json!({"event_id": r.event_id, "unverified": r.unverified})),
    });
    // 發布的結果（重新錨定後的保持期間為 false）；寫入稽核的結果是鏈本身的驗證結果
    let published_ok = ok;
    let ok = chain_ok;
    let now = now_ms;
    let r = db
        .write(move |conn| {
            conn.audit_append(
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
    Some(published_ok)
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
        "reanchor_mismatch" => "重新錨定的紀錄被刪除或修改，或指標被改寫",
        "reanchored" => "稽核鏈已由管理者重新錨定（之前的紀錄完整性未驗證）",
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
    // 每分鐘看一次有沒有新的重新錨定（CLI 在另一個行程執行）：有的話立刻驗證並發布，不等下一次定期驗證
    let mut next_poll = start + FIRST_RUN + REANCHOR_POLL;
    let mut seen_reanchor: Option<Option<i64>> = None;
    loop {
        let wake = next_anchor
            .map_or(next_verify, |a| a.min(next_verify))
            .min(next_poll);
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
        if now >= next_poll {
            next_poll = now + REANCHOR_POLL;
            if let Ok(point) = db.read(|c| Ok(c.audit_reanchor_point()?)).await {
                let id = point.map(|p| p.event_id);
                if seen_reanchor.is_some_and(|s| s != id) && id.is_some() {
                    next_verify = now;
                }
                seen_reanchor = Some(id);
            }
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
                c.audit_append(i, &ev("user.create"))?;
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
            c.test_sqlite()
                .execute("UPDATE audit_events SET object = 'eve' WHERE id = 2", [])?;
            let rows: Vec<audit::AuditRow> =
                c.audit_export_batch(&audit::AuditFilter::default(), 100)?;
            let mut prev = audit::GENESIS.to_string();
            for r in rows {
                let h = audit::compute_hash(&prev, &r);
                c.test_sqlite().execute(
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
                Ok(c.test_sqlite().query_row(
                    "SELECT count(*) FROM audit_events WHERE action = 'audit.verify' AND actor = 'system'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(n, 2);
        log_anchor(&db).await;
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // 重新錨定之後的 24 小時內，自我驗證把「稽核日誌完整性」保持在不通過（原因 reanchored）
    #[tokio::test]
    async fn a_recent_reanchor_keeps_the_integrity_condition_open() {
        let (dir, db) = temp_db();
        audit::ensure_key(&dir).unwrap();
        db.write(|c| {
            for i in 0..3 {
                c.audit_append(i, &ev("user.create"))?;
            }
            c.test_sqlite()
                .execute("UPDATE audit_events SET object = 'eve' WHERE id = 2", [])?;
            Ok(())
        })
        .await
        .unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        assert_eq!(self_verify(&db, &bus).await, Some(false));
        let _ = rx.recv().await.unwrap();
        let reanchor = |at: i64| {
            let db = db.clone();
            async move {
                db.write(move |c| {
                    let e = audit::Audit {
                        actor: "ops",
                        category: "audit",
                        action: audit::ACTION_REANCHOR,
                        result: "ok",
                        ..Default::default()
                    };
                    Ok(c.audit_reanchor(at, &e)?.expect("有金鑰"))
                })
                .await
                .unwrap()
            }
        };
        let now = chrono::Utc::now().timestamp_millis();
        // 超過保持期間的重新錨定：鏈從新的起點起算，通過
        reanchor(now - audit::REANCHOR_ALERT_HOLD_MS - 60_000).await;
        assert_eq!(self_verify(&db, &bus).await, Some(true));
        let _ = rx.recv().await.unwrap();
        // 剛剛的重新錨定：不通過，原因 reanchored，指向那一筆事件
        let id = reanchor(now).await;
        assert_eq!(self_verify(&db, &bus).await, Some(false));
        match &rx.recv().await.unwrap().kind {
            EventKind::AuditIntegrity {
                ok,
                reason,
                id: at_id,
                summary,
            } => {
                assert!(!ok);
                assert_eq!(reason.as_deref(), Some("reanchored"));
                assert_eq!(*at_id, Some(id));
                assert!(summary.contains("重新錨定"), "{summary}");
            }
            other => panic!("{other:?}"),
        }
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_break_reason_has_a_description() {
        for r in [
            "reanchor_mismatch",
            "reanchored",
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
