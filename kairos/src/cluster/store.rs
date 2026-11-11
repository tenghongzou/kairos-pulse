//! 叢集協調的領域 store（[`ClusterStore`]；`kairos/HA.md` 第 5.5、6.1、6.2 節）。
//!
//! 節點（`cluster_nodes`）與租約（`cluster_leases`）的每個操作都是**單一敘述**，時間一律取自資料庫
//! （PostgreSQL 的 `clock_timestamp()`），節點的時鐘不影響互斥。`now` 參數只給測試用（可控的資料庫時間）：
//! 正式程式一律傳 `None`。SQLite 後端也實作（時間取自 SQLite 的 `unixepoch`），但叢集模式只支援
//! PostgreSQL；SQLite 的實作用於單元測試與 `kairos cluster status` 在單機上的輸出。
//!
//! 兩個後端的 SQL 相同（[`sql`]），只有「現在」的運算式不同。

use crate::store::db::StoreResult;

/// leader 租約的名稱（`cluster_leases.name`；v10 只有這一列）
pub const LEASE_LEADER: &str = "leader";

/// 節點的角色（`cluster_nodes.role`）
pub mod role {
    /// 已註冊，還沒有通過金鑰與 schema 的檢查
    pub const STARTING: &str = "starting";
    /// 備援：連著資料庫、嘗試取得租約，不監聽
    pub const STANDBY: &str = "standby";
    /// 持有 leader 租約（階段 A：主動節點）
    pub const LEADER: &str = "leader";
    /// 排空中：不參與 leader 選舉（`kairos cluster drain`）
    pub const DRAINING: &str = "draining";
    /// 正常關機
    pub const STOPPED: &str = "stopped";
    /// 心跳逾時，由 leader 標記（`KRS-W8506`）
    pub const LOST: &str = "lost";
}

/// `cluster_nodes` 的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub node_id: String,
    pub hostname: String,
    pub version: String,
    pub schema_version: i64,
    pub role: String,
    pub config_hash: String,
    pub keys_hash: String,
    pub advertise: Option<String>,
    pub started_at: i64,
    pub heartbeat_at: i64,
    pub info: Option<String>,
    /// 讀取當下（資料庫時間）距離最後一次心跳的毫秒數
    pub heartbeat_age_ms: i64,
}

/// `cluster_leases` 的一列，另帶讀取當下的資料庫時間
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRow {
    pub holder: String,
    pub epoch: i64,
    pub acquired_at: i64,
    pub expires_at: i64,
    /// 敘述執行時的資料庫時間（Unix 毫秒）。SQLite 的 `RETURNING` 不能引用 `FROM` 的子查詢，所以續約與
    /// 交還時由回傳的 `expires_at` 推回（續約：`expires_at − ttl`；交還：`expires_at + 1`）
    pub now: i64,
}

impl LeaseRow {
    /// 租約仍有效（還沒到期）
    pub fn is_live(&self) -> bool {
        self.expires_at > self.now
    }
}

/// 節點註冊（啟動時 UPSERT）的內容
#[derive(Debug, Clone)]
pub struct NodeRegistration<'a> {
    pub node_id: &'a str,
    pub hostname: &'a str,
    pub version: &'a str,
    pub schema_version: i64,
    pub config_hash: &'a str,
    pub keys_hash: &'a str,
    pub advertise: Option<&'a str>,
    pub info: Option<&'a str>,
}

/// 叢集協調的資料庫操作
pub trait ClusterStore {
    /// 一個節點的紀錄
    fn cluster_node(&self, node_id: &str, now: Option<i64>) -> StoreResult<Option<NodeRow>>;

    /// 所有節點（依 `node_id` 排序）
    fn cluster_nodes(&self, now: Option<i64>) -> StoreResult<Vec<NodeRow>>;

    /// 註冊節點（UPSERT，角色為 `starting`）；回傳這次啟動的 `started_at`（資料庫時間，之後的心跳以它
    /// 辨識「還是同一次啟動」）
    fn cluster_node_register(
        &self,
        reg: &NodeRegistration<'_>,
        now: Option<i64>,
    ) -> StoreResult<i64>;

    /// 心跳：更新 `heartbeat_at`、`role`、`keys_hash`、`info`。資料庫中的角色是 `draining` 時保留
    /// （除非新角色是 `stopped`）。回傳更新後的角色；紀錄不存在或 `started_at` 不同（同名的另一個行程
    /// 已重新註冊）時為 `None`
    fn cluster_node_heartbeat(
        &self,
        node_id: &str,
        started_at: i64,
        role: &str,
        keys_hash: &str,
        info: Option<&str>,
        now: Option<i64>,
    ) -> StoreResult<Option<String>>;

    /// 直接設定節點的角色（`kairos cluster drain`）；回傳是否有這個節點
    fn cluster_node_set_role(&self, node_id: &str, role: &str) -> StoreResult<bool>;

    /// 刪除節點的紀錄（`kairos cluster forget`）
    fn cluster_node_forget(&self, node_id: &str) -> StoreResult<bool>;

    /// 把心跳超過 `lost_after_ms` 的節點（`except` 以外、還沒有標為 `stopped`／`lost` 的）標為 `lost`；
    /// 回傳被標記的節點
    fn cluster_mark_lost(
        &self,
        lost_after_ms: i64,
        except: &str,
        now: Option<i64>,
    ) -> StoreResult<Vec<String>>;

    /// 讀取租約
    fn cluster_lease(&self, name: &str, now: Option<i64>) -> StoreResult<Option<LeaseRow>>;

    /// 嘗試取得租約：沒有人持有、已到期、或持有者就是 `node`（同一個節點重新啟動）時取得，`epoch` 加一。
    /// 被其他節點持有且未到期時回 `None`
    fn cluster_lease_acquire(
        &self,
        name: &str,
        node: &str,
        ttl_ms: i64,
        now: Option<i64>,
    ) -> StoreResult<Option<LeaseRow>>;

    /// 續約：只有「持有者與 epoch 都相同、且尚未到期」才成功；否則回 `None`（已失去租約）
    fn cluster_lease_renew(
        &self,
        name: &str,
        node: &str,
        epoch: i64,
        ttl_ms: i64,
        now: Option<i64>,
    ) -> StoreResult<Option<LeaseRow>>;

    /// 持有者主動交還（正常關機、排空）：把到期時間改為現在之前；回傳是否交還了
    fn cluster_lease_release(
        &self,
        name: &str,
        node: &str,
        epoch: i64,
        now: Option<i64>,
    ) -> StoreResult<bool>;

    /// 強制讓目前的租約到期（`kairos cluster step-down`）；回傳原本的持有者（租約不存在或已到期時為 `None`）
    fn cluster_lease_vacate(&self, name: &str, now: Option<i64>) -> StoreResult<Option<LeaseRow>>;
}

/// 兩個後端共用的 SQL。`now` 是「現在」的運算式（[`now_expr`]）
pub(crate) mod sql {
    /// 「現在」（Unix 毫秒）：第 `param` 個參數不是 NULL 時用它（測試），否則是資料庫的時間
    pub fn now_expr(postgres: bool, param: usize) -> String {
        if postgres {
            format!(
                "COALESCE(CAST(?{param} AS BIGINT), (extract(epoch FROM clock_timestamp()) * 1000)::bigint)"
            )
        } else {
            format!(
                "COALESCE(CAST(?{param} AS INTEGER), CAST(unixepoch('subsec') * 1000 AS INTEGER))"
            )
        }
    }

    pub const NODE_COLUMNS: &str = "node_id, hostname, version, schema_version, role, config_hash, \
         keys_hash, advertise, started_at, heartbeat_at, info";

    /// ?1 node_id（NULL＝全部）、?2 now
    pub fn nodes(postgres: bool) -> String {
        format!(
            "SELECT {NODE_COLUMNS}, t.now - heartbeat_at
             FROM cluster_nodes, (SELECT {now} AS now) t
             WHERE CAST(?1 AS TEXT) IS NULL OR node_id = CAST(?1 AS TEXT)
             ORDER BY node_id",
            now = now_expr(postgres, 2)
        )
    }

    /// ?1 node_id ?2 hostname ?3 version ?4 schema_version ?5 config_hash ?6 keys_hash ?7 advertise
    /// ?8 info ?9 now
    pub fn register(postgres: bool) -> String {
        format!(
            "INSERT INTO cluster_nodes ({NODE_COLUMNS})
             SELECT CAST(?1 AS TEXT), CAST(?2 AS TEXT), CAST(?3 AS TEXT), CAST(?4 AS {int}),
                    'starting', CAST(?5 AS TEXT), CAST(?6 AS TEXT), CAST(?7 AS TEXT), t.now, t.now,
                    CAST(?8 AS TEXT)
             FROM (SELECT {now} AS now) t WHERE true
             ON CONFLICT (node_id) DO UPDATE SET
                 hostname = excluded.hostname, version = excluded.version,
                 schema_version = excluded.schema_version, role = excluded.role,
                 config_hash = excluded.config_hash, keys_hash = excluded.keys_hash,
                 advertise = excluded.advertise, started_at = excluded.started_at,
                 heartbeat_at = excluded.heartbeat_at, info = excluded.info
             RETURNING started_at",
            int = if postgres { "BIGINT" } else { "INTEGER" },
            now = now_expr(postgres, 9)
        )
    }

    /// ?1 node_id ?2 started_at ?3 role ?4 keys_hash ?5 info ?6 now
    pub fn heartbeat(postgres: bool) -> String {
        format!(
            "UPDATE cluster_nodes SET
                 heartbeat_at = t.now,
                 role = CASE WHEN cluster_nodes.role = 'draining' AND CAST(?3 AS TEXT) <> 'stopped'
                             THEN 'draining' ELSE CAST(?3 AS TEXT) END,
                 keys_hash = CAST(?4 AS TEXT),
                 info = CAST(?5 AS TEXT)
             FROM (SELECT {now} AS now) t
             WHERE node_id = ?1 AND started_at = ?2
             RETURNING cluster_nodes.role",
            now = now_expr(postgres, 6)
        )
    }

    /// ?1 lost_after_ms ?2 except ?3 now
    pub fn mark_lost(postgres: bool) -> String {
        format!(
            "UPDATE cluster_nodes SET role = 'lost'
             FROM (SELECT {now} AS now) t
             WHERE cluster_nodes.role NOT IN ('stopped', 'lost')
               AND heartbeat_at < t.now - ?1 AND node_id <> ?2
             RETURNING node_id",
            now = now_expr(postgres, 3)
        )
    }

    /// ?1 name ?2 now
    pub fn lease(postgres: bool) -> String {
        format!(
            "SELECT holder, epoch, acquired_at, expires_at, t.now
             FROM cluster_leases, (SELECT {now} AS now) t WHERE name = ?1",
            now = now_expr(postgres, 2)
        )
    }

    /// ?1 name ?2 node ?3 ttl_ms ?4 now
    pub fn acquire(postgres: bool) -> String {
        format!(
            "INSERT INTO cluster_leases (name, holder, epoch, acquired_at, expires_at)
             SELECT CAST(?1 AS TEXT), CAST(?2 AS TEXT), 1, t.now, t.now + ?3
             FROM (SELECT {now} AS now) t WHERE true
             ON CONFLICT (name) DO UPDATE SET
                 epoch = cluster_leases.epoch + 1,
                 acquired_at = excluded.acquired_at,
                 holder = excluded.holder,
                 expires_at = excluded.expires_at
             WHERE cluster_leases.holder = excluded.holder
                OR cluster_leases.expires_at < excluded.acquired_at
             RETURNING holder, epoch, acquired_at, expires_at, acquired_at",
            now = now_expr(postgres, 4)
        )
    }

    /// ?1 name ?2 node ?3 epoch ?4 ttl_ms ?5 now
    pub fn renew(postgres: bool) -> String {
        format!(
            "UPDATE cluster_leases SET expires_at = t.now + ?4
             FROM (SELECT {now} AS now) t
             WHERE name = ?1 AND holder = ?2 AND epoch = ?3 AND expires_at > t.now
             RETURNING holder, epoch, acquired_at, expires_at, expires_at - ?4",
            now = now_expr(postgres, 5)
        )
    }

    /// ?1 name ?2 node ?3 epoch ?4 now
    pub fn release(postgres: bool) -> String {
        format!(
            "UPDATE cluster_leases SET expires_at = t.now - 1
             FROM (SELECT {now} AS now) t
             WHERE name = ?1 AND holder = ?2 AND epoch = ?3 AND expires_at >= t.now",
            now = now_expr(postgres, 4)
        )
    }

    /// ?1 name ?2 now
    pub fn vacate(postgres: bool) -> String {
        format!(
            "UPDATE cluster_leases SET expires_at = t.now - 1
             FROM (SELECT {now} AS now) t
             WHERE name = ?1 AND expires_at >= t.now
             RETURNING holder, epoch, acquired_at, expires_at, expires_at + 1",
            now = now_expr(postgres, 2)
        )
    }
}
