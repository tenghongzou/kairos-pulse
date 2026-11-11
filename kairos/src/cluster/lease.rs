//! leader 租約的續約與自我降級（`kairos/HA.md` 第 6.2 節）。
//!
//! [`LeaseKeeper`] 不自己讀時鐘、不持有連線：呼叫端把單調時鐘（[`Instant`]）與資料庫操作
//! （[`ClusterStore`]）傳進來，所以可以用假時鐘測試「租約到期」「被別人取得」「資料庫沒有回應」。
//!
//! 規則：
//!
//! - **續約**是嚴格的：只有「持有者與 epoch 都相同、且尚未到期」才成功（`cluster_lease_renew`）。
//!   沒有回傳列＝租約已被別人取得、已被 `kairos cluster step-down` 交還、或已到期 → [`Renew::Lost`]。
//! - **自我降級**：以本機的單調時鐘記錄「最後一次續約成功的**送出**時間」。距今超過
//!   `lease_ttl_secs − 5` 秒仍沒有成功續約時視為已失去租約（[`LeaseKeeper::overdue`]）——即使資料庫
//!   完全沒有回應也成立，而且一定早於租約在資料庫中到期（別的節點最早在那之後才能接手）。

use std::time::{Duration, Instant};

use super::store::{ClusterStore, LEASE_LEADER};
use crate::store::db::StoreError;

/// 一次續約的結果
#[derive(Debug)]
pub enum Renew {
    /// 續約成功（新的到期時間，資料庫時間）
    Renewed { expires_at: i64 },
    /// 已失去租約（被別人取得、被交還或到期）
    Lost,
    /// 資料庫錯誤：結果不明，稍後重試（[`LeaseKeeper::overdue`] 決定何時放棄）
    Error(StoreError),
}

/// 持有租約的節點的續約狀態
#[derive(Debug, Clone)]
pub struct LeaseKeeper {
    node: String,
    epoch: i64,
    ttl: Duration,
    demote_after: Duration,
    /// 最後一次成功的續約（或取得）的送出時間
    last_ok_sent: Instant,
}

impl LeaseKeeper {
    /// `acquired_sent`：取得租約的敘述送出的時間（不是收到回應的時間：保守地早算）
    pub fn new(
        node: impl Into<String>,
        epoch: i64,
        ttl: Duration,
        demote_after: Duration,
        acquired_sent: Instant,
    ) -> Self {
        Self {
            node: node.into(),
            epoch,
            ttl,
            demote_after,
            last_ok_sent: acquired_sent,
        }
    }

    pub fn epoch(&self) -> i64 {
        self.epoch
    }

    /// 距離最後一次成功續約的送出時間已超過自我降級的門檻
    pub fn overdue(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_ok_sent) > self.demote_after
    }

    /// 最晚要在什麼時候之前續約成功
    pub fn deadline(&self) -> Instant {
        self.last_ok_sent + self.demote_after
    }

    /// 續約一次。`sent` 是送出敘述之前讀到的單調時鐘；`db_now` 只給測試（可控的資料庫時間）
    pub fn renew(&mut self, store: &dyn ClusterStore, sent: Instant, db_now: Option<i64>) -> Renew {
        let ttl_ms = i64::try_from(self.ttl.as_millis()).unwrap_or(i64::MAX);
        match store.cluster_lease_renew(LEASE_LEADER, &self.node, self.epoch, ttl_ms, db_now) {
            Ok(Some(row)) => {
                self.last_ok_sent = sent;
                Renew::Renewed {
                    expires_at: row.expires_at,
                }
            }
            Ok(None) => Renew::Lost,
            Err(e) => Renew::Error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::store::{LeaseRow, NodeRegistration};
    use crate::store::db::Store;

    const TTL: Duration = Duration::from_secs(15);
    const DEMOTE: Duration = Duration::from_secs(10);
    const TTL_MS: i64 = 15_000;

    /// 記憶體中的 SQLite（schema 與正式的相同）
    fn db() -> rusqlite::Connection {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::sqlite::migrate(&mut conn, crate::store::sqlite::MIGRATIONS).unwrap();
        conn
    }

    fn acquire(s: &dyn Store, node: &str, now: i64) -> Option<LeaseRow> {
        s.cluster_lease_acquire(LEASE_LEADER, node, TTL_MS, Some(now))
            .unwrap()
    }

    #[test]
    fn first_node_acquires_and_the_second_waits_until_expiry() {
        // 假時鐘：資料庫時間由參數指定
        let conn = db();
        let t0 = 1_000_000;
        let a = acquire(&conn, "kairos-a", t0).expect("第一個節點取得租約");
        assert_eq!((a.holder.as_str(), a.epoch), ("kairos-a", 1));
        assert_eq!(a.expires_at, t0 + TTL_MS);
        // 未到期：另一個節點取不到
        assert!(acquire(&conn, "kairos-b", t0 + 1).is_none());
        assert!(acquire(&conn, "kairos-b", t0 + TTL_MS).is_none());
        // 到期之後接手，epoch 加一
        let b = acquire(&conn, "kairos-b", t0 + TTL_MS + 1).expect("到期後接手");
        assert_eq!((b.holder.as_str(), b.epoch), ("kairos-b", 2));
        // 舊 leader 的續約失敗（持有者已改變）
        let mut keeper = LeaseKeeper::new("kairos-a", 1, TTL, DEMOTE, Instant::now());
        assert!(matches!(
            keeper.renew(&conn, Instant::now(), Some(t0 + TTL_MS + 2)),
            Renew::Lost
        ));
        // 舊 leader 也不能以「重新取得」蓋掉新 leader（未到期）
        assert!(acquire(&conn, "kairos-a", t0 + TTL_MS + 3).is_none());
        let lease = conn
            .cluster_lease(LEASE_LEADER, Some(t0 + TTL_MS + 3))
            .unwrap()
            .unwrap();
        assert_eq!((lease.holder.as_str(), lease.epoch), ("kairos-b", 2));
        assert!(lease.is_live());
    }

    #[test]
    fn renewal_extends_the_lease_and_keeps_the_epoch() {
        let conn = db();
        let t0 = 5_000_000;
        let start = Instant::now();
        let a = acquire(&conn, "kairos-a", t0).unwrap();
        let mut keeper = LeaseKeeper::new("kairos-a", a.epoch, TTL, DEMOTE, start);
        for i in 1..=10_i64 {
            let sent = start + Duration::from_secs(5 * i as u64);
            match keeper.renew(&conn, sent, Some(t0 + 5_000 * i)) {
                Renew::Renewed { expires_at } => {
                    assert_eq!(expires_at, t0 + 5_000 * i + TTL_MS);
                }
                other => panic!("第 {i} 次續約應成功：{other:?}"),
            }
            assert!(!keeper.overdue(sent));
            // 續約期間別的節點一直取不到
            assert!(acquire(&conn, "kairos-b", t0 + 5_000 * i + 1).is_none());
        }
    }

    #[test]
    fn an_expired_lease_cannot_be_renewed_even_if_nobody_took_it() {
        // 續約敘述帶「尚未到期」的條件：到期之後 leader 必須重新取得（epoch 加一），不能默默續下去
        let conn = db();
        let t0 = 9_000_000;
        let a = acquire(&conn, "kairos-a", t0).unwrap();
        let mut keeper = LeaseKeeper::new("kairos-a", a.epoch, TTL, DEMOTE, Instant::now());
        assert!(matches!(
            keeper.renew(&conn, Instant::now(), Some(t0 + TTL_MS + 1)),
            Renew::Lost
        ));
        // 同一個節點重新取得：epoch 加一（新的 fencing token）
        let again = acquire(&conn, "kairos-a", t0 + TTL_MS + 2).unwrap();
        assert_eq!(again.epoch, 2);
    }

    #[test]
    fn the_leader_demotes_itself_before_the_lease_can_be_taken() {
        // 資料庫沒有回應（續約一直沒有成功）：TTL − 5 秒後自我降級，早於租約到期
        let start = Instant::now();
        let keeper = LeaseKeeper::new("kairos-a", 1, TTL, DEMOTE, start);
        assert!(!keeper.overdue(start + Duration::from_secs(9)));
        assert!(!keeper.overdue(start + DEMOTE));
        assert!(keeper.overdue(start + DEMOTE + Duration::from_millis(1)));
        assert!(keeper.deadline() < start + TTL);
    }

    #[test]
    fn two_nodes_racing_for_a_free_lease_produce_one_leader() {
        // 同一個時間點兩個節點都嘗試取得：單一敘述，只有一個成功
        let conn = db();
        let t0 = 2_000_000;
        let results: Vec<_> = ["kairos-a", "kairos-b", "kairos-c"]
            .iter()
            .map(|n| acquire(&conn, n, t0))
            .collect();
        assert_eq!(results.iter().filter(|r| r.is_some()).count(), 1);
        assert_eq!(results[0].as_ref().unwrap().holder, "kairos-a");
    }

    #[test]
    fn step_down_and_release_let_a_standby_take_over_immediately() {
        let conn = db();
        let t0 = 3_000_000;
        let a = acquire(&conn, "kairos-a", t0).unwrap();
        // kairos cluster step-down：強制到期，回傳原本的持有者
        let vacated = conn
            .cluster_lease_vacate(LEASE_LEADER, Some(t0 + 100))
            .unwrap()
            .unwrap();
        assert_eq!(vacated.holder, "kairos-a");
        // leader 的下一次續約失敗（不會把租約續回來）
        let mut keeper = LeaseKeeper::new("kairos-a", a.epoch, TTL, DEMOTE, Instant::now());
        assert!(matches!(
            keeper.renew(&conn, Instant::now(), Some(t0 + 200)),
            Renew::Lost
        ));
        // 備援不必等 TTL
        let b = acquire(&conn, "kairos-b", t0 + 300).unwrap();
        assert_eq!((b.holder.as_str(), b.epoch), ("kairos-b", 2));
        // 正常關機的交還：只有持有者（epoch 相同）可以交還
        assert!(
            !conn
                .cluster_lease_release(LEASE_LEADER, "kairos-a", 1, Some(t0 + 400))
                .unwrap()
        );
        assert!(
            conn.cluster_lease_release(LEASE_LEADER, "kairos-b", 2, Some(t0 + 400))
                .unwrap()
        );
        let c = acquire(&conn, "kairos-a", t0 + 401).unwrap();
        assert_eq!(c.epoch, 3);
        // 已經到期的租約再 step-down：沒有東西可以交還
        assert!(
            conn.cluster_lease_vacate(LEASE_LEADER, Some(t0 + 401 + TTL_MS + 1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn nodes_register_heartbeat_and_are_marked_lost() {
        let conn = db();
        let t0 = 7_000_000;
        let reg = |id: &'static str| NodeRegistration {
            node_id: id,
            hostname: id,
            version: "kairos test",
            schema_version: 10,
            config_hash: "c",
            keys_hash: "k",
            advertise: None,
            info: None,
        };
        let a = conn
            .cluster_node_register(&reg("kairos-a"), Some(t0))
            .unwrap();
        let b = conn
            .cluster_node_register(&reg("kairos-b"), Some(t0))
            .unwrap();
        assert_eq!((a, b), (t0, t0));
        assert_eq!(
            conn.cluster_node_heartbeat("kairos-a", a, "leader", "k", None, Some(t0 + 5_000))
                .unwrap()
                .as_deref(),
            Some("leader")
        );
        // 另一個行程以同一個 node_id 重新註冊之後，舊行程的心跳不再生效
        let a2 = conn
            .cluster_node_register(&reg("kairos-a"), Some(t0 + 6_000))
            .unwrap();
        assert_eq!(
            conn.cluster_node_heartbeat("kairos-a", a, "leader", "k", None, Some(t0 + 7_000))
                .unwrap(),
            None
        );
        conn.cluster_node_heartbeat("kairos-a", a2, "leader", "k", None, Some(t0 + 20_000))
            .unwrap();
        // kairos-b 的心跳停在 t0：超過 20 秒後由 leader 標為失聯（自己除外）
        let lost = conn
            .cluster_mark_lost(20_000, "kairos-a", Some(t0 + 20_001))
            .unwrap();
        assert_eq!(lost, vec!["kairos-b".to_string()]);
        assert!(
            conn.cluster_mark_lost(20_000, "kairos-a", Some(t0 + 20_002))
                .unwrap()
                .is_empty()
        );
        let nodes = conn.cluster_nodes(Some(t0 + 20_001)).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].role, "lost");
        assert_eq!(nodes[1].heartbeat_age_ms, 20_001);
        // 排空：資料庫中的 draining 不會被心跳蓋掉，除非節點結束
        assert!(conn.cluster_node_set_role("kairos-a", "draining").unwrap());
        assert_eq!(
            conn.cluster_node_heartbeat("kairos-a", a2, "leader", "k", None, Some(t0 + 21_000))
                .unwrap()
                .as_deref(),
            Some("draining")
        );
        assert_eq!(
            conn.cluster_node_heartbeat("kairos-a", a2, "stopped", "k", None, Some(t0 + 22_000))
                .unwrap()
                .as_deref(),
            Some("stopped")
        );
        assert!(conn.cluster_node_forget("kairos-b").unwrap());
        assert!(!conn.cluster_node_forget("kairos-b").unwrap());
        assert!(conn.cluster_node("kairos-b", None).unwrap().is_none());
    }
}
