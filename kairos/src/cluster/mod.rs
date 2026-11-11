//! 叢集協調（高可用，階段 A：主動／備援；`kairos/HA.md` 第 3.2、6 節）。
//!
//! 兩個以上的 Kairos 共用一個 PostgreSQL，以資料庫中的租約列（`cluster_leases`）選出唯一的 leader：
//!
//! - **備援節點**（沒有租約）：只連著資料庫、寫心跳、每 2 秒嘗試取得租約，**不開啟儲存、不監聽**
//!   agent、註冊與主控台的連接埠（只有 `observability.listen` 上的 `/healthz`、`/readyz`、`/metrics`）。
//! - **取得租約**後才依單機的啟動流程載入狀態、開始監聽（`main::run` 的其餘部分）。因此所有背景工作
//!   （告警、彙總、保留、CRL、稽核自我驗證、遠端管理的推送引擎……）天然只在 leader 上執行。
//! - **失去租約**（續約失敗超過 `lease_ttl_secs − 5` 秒、發現租約已被別人取得、或任何寫入的 fencing
//!   檢查失敗）時立即以結束代碼 [`EXIT_LEASE_LOST`] 結束行程，由服務管理員重新啟動為備援。
//! - **fencing**：leader 的每個可寫交易與自動提交的寫入敘述都先以 `FOR SHARE` 確認租約
//!   （[`crate::store::postgres::pool::PgPool::set_fence`]），所以舊 leader 的寫入與新 leader 取得租約不會重疊。
//!
//! 模組：
//!
//! - [`store`]：`ClusterStore`（節點與租約的資料庫操作，兩個後端）。
//! - [`lease`]：續約與自我降級的判斷（不依賴時鐘與連線，可以用假時鐘測試）。
//! - [`keys`]：金鑰指紋、叢集模式下停用金鑰的自動建立。
//! - [`confhash`]：需要一致的設定的雜湊。
//! - [`runtime`]：協調執行緒（心跳、取得與續約、節點失聯、交還）。
//!
//! 備援節點的 `/healthz`、`/readyz`、`/metrics` 在 [`crate::probe`]（`StandbyProbe`）。

pub mod confhash;
pub mod keys;
pub mod lease;
pub mod runtime;
pub mod store;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use self::store::NodeRow;
use crate::config::ClusterSettings;

/// 本節點取得 leader 租約
pub const CODE_LEADER_ACQUIRED: &str = "KRS-I8501";
/// 本節點失去 leader 租約
pub const CODE_LEASE_LOST: &str = "KRS-W8502";
/// 叢集的協調異常：沒有 leader，或 `node_id` 重複
pub const CODE_COORDINATION: &str = "KRS-E8503";
/// 本節點的金鑰與叢集不一致或缺少
pub const CODE_KEYS: &str = "KRS-E8504";
/// 叢集節點的設定不一致
pub const CODE_CONFIG_DRIFT: &str = "KRS-W8505";
/// 叢集節點失聯
pub const CODE_NODE_LOST: &str = "KRS-W8506";

/// 失去租約時的結束代碼（`kairos/HA.md` 第 3.2 節；`EX_TEMPFAIL`）：服務管理員應重新啟動
pub const EXIT_LEASE_LOST: u8 = 75;

/// `meta` 的鍵：叢集的 UUID（`kairos cluster init` 產生）
pub const META_CLUSTER_ID: &str = "cluster_id";
/// `meta` 的鍵：各金鑰的指紋（JSON，見 [`keys`]）
pub const META_CLUSTER_KEYS: &str = "cluster_keys";

/// `LISTEN/NOTIFY` 的頻道：租約或節點狀態改變（只用來縮短延遲）
pub const NOTIFY_CHANNEL: &str = "kairos_cluster";

/// 本節點在叢集中的狀態（`/readyz` 的 `cluster`、`/metrics` 的 `kairos_cluster_role`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    /// 還沒有完成註冊（或金鑰、schema 檢查沒有通過）
    Starting = 0,
    /// 備援：嘗試取得租約
    Standby = 1,
    /// 持有租約
    Leader = 2,
    /// 排空中：不參與選舉
    Draining = 3,
    /// 連不上資料庫，或租約狀態不明
    Lost = 4,
    /// 正在結束
    Stopped = 5,
}

impl Role {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Role::Standby,
            2 => Role::Leader,
            3 => Role::Draining,
            4 => Role::Lost,
            5 => Role::Stopped,
            _ => Role::Starting,
        }
    }

    /// `cluster_nodes.role` 的值
    pub fn as_db(self) -> &'static str {
        match self {
            Role::Starting | Role::Lost => store::role::STARTING,
            Role::Standby => store::role::STANDBY,
            Role::Leader => store::role::LEADER,
            Role::Draining => store::role::DRAINING,
            Role::Stopped => store::role::STOPPED,
        }
    }

    /// `/readyz` 的 `cluster` 狀態字（`kairos/HA.md` 第 14.1 節）
    pub fn as_check(self) -> &'static str {
        match self {
            Role::Leader => "leader",
            Role::Standby | Role::Starting => "standby",
            Role::Draining => "draining",
            Role::Lost => "lost",
            Role::Stopped => "stopped",
        }
    }
}

/// schema 的檢查結果（`/readyz` 的 `schema`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SchemaCheck {
    Ok = 0,
    /// 資料庫的 schema 比這個 Kairos 新（`KRS-E4405`）
    Newer = 1,
    /// 資料庫的 schema 較舊（等待升級）
    Older = 2,
}

/// 目前的 leader（本節點看到的）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderInfo {
    pub node_id: String,
    pub epoch: i64,
    /// 取得租約的時間（資料庫時間，Unix 毫秒）
    pub since: i64,
}

/// 本節點取得租約時的上一個持有者（告警的換手判斷、SSE 的 `resync` 原因）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Takeover {
    /// 上一個持有者；第一次建立租約時為 `None`
    pub previous: Option<String>,
    pub epoch: i64,
}

/// 叢集協調的共用狀態（協調執行緒寫入；探針、系統健康、`/metrics`、連線讀取）
#[derive(Debug)]
pub struct ClusterState {
    pub node_id: String,
    pub settings: ClusterSettings,
    role: AtomicU8,
    schema: AtomicU8,
    keys_ok: AtomicBool,
    db_ok: AtomicBool,
    lease_lost: AtomicBool,
    /// 目前看到的 leader epoch
    pub epoch: AtomicI64,
    pub renew_ok: AtomicU64,
    pub renew_lost: AtomicU64,
    pub renew_error: AtomicU64,
    pub leader_changes: AtomicU64,
    cluster_id: Mutex<Option<String>>,
    leader: Mutex<Option<LeaderInfo>>,
    takeover: Mutex<Option<Takeover>>,
    nodes: Mutex<Vec<NodeRow>>,
    key_problems: Mutex<Vec<String>>,
    /// 需要一致的設定的雜湊（本節點）
    pub config_hash: String,
    /// 本節點的線上 agent 連線數（成為 leader 後由 `main::run` 提供；寫入 `cluster_nodes.info`）
    connections: ConnGauge,
}

/// 讀取線上連線數的函式（只為了讓 [`ClusterState`] 可以 `Debug`）
#[derive(Default)]
struct ConnGauge(OnceLock<Box<dyn Fn() -> usize + Send + Sync>>);

impl std::fmt::Debug for ConnGauge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnGauge(..)")
    }
}

impl ClusterState {
    pub fn new(node_id: String, settings: ClusterSettings, config_hash: String) -> Self {
        Self {
            node_id,
            settings,
            role: AtomicU8::new(Role::Starting as u8),
            schema: AtomicU8::new(SchemaCheck::Ok as u8),
            keys_ok: AtomicBool::new(false),
            db_ok: AtomicBool::new(false),
            lease_lost: AtomicBool::new(false),
            epoch: AtomicI64::new(0),
            renew_ok: AtomicU64::new(0),
            renew_lost: AtomicU64::new(0),
            renew_error: AtomicU64::new(0),
            leader_changes: AtomicU64::new(0),
            cluster_id: Mutex::new(None),
            leader: Mutex::new(None),
            takeover: Mutex::new(None),
            nodes: Mutex::new(Vec::new()),
            key_problems: Mutex::new(Vec::new()),
            config_hash,
            connections: ConnGauge::default(),
        }
    }

    /// 提供「線上 agent 連線數」的讀取方式（只能設定一次）
    pub fn set_connections(&self, f: impl Fn() -> usize + Send + Sync + 'static) {
        let _ = self.connections.0.set(Box::new(f));
    }

    /// 本節點的線上 agent 連線數（備援節點為 0）
    pub fn connections(&self) -> usize {
        self.connections.0.get().map_or(0, |f| f())
    }

    pub fn role(&self) -> Role {
        Role::from_u8(self.role.load(Ordering::SeqCst))
    }

    pub fn set_role(&self, role: Role) {
        self.role.store(role as u8, Ordering::SeqCst);
    }

    pub fn is_leader(&self) -> bool {
        self.role() == Role::Leader && !self.lease_lost()
    }

    pub fn schema(&self) -> SchemaCheck {
        match self.schema.load(Ordering::SeqCst) {
            1 => SchemaCheck::Newer,
            2 => SchemaCheck::Older,
            _ => SchemaCheck::Ok,
        }
    }

    pub fn set_schema(&self, s: SchemaCheck) {
        self.schema.store(s as u8, Ordering::SeqCst);
    }

    pub fn keys_ok(&self) -> bool {
        self.keys_ok.load(Ordering::SeqCst)
    }

    /// 金鑰檢查的結果（`problems` 是空的＝通過）
    pub fn set_keys(&self, problems: Vec<String>) {
        self.keys_ok.store(problems.is_empty(), Ordering::SeqCst);
        *self.key_problems.lock().unwrap_or_else(|e| e.into_inner()) = problems;
    }

    pub fn key_problems(&self) -> Vec<String> {
        self.key_problems
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn db_ok(&self) -> bool {
        self.db_ok.load(Ordering::SeqCst)
    }

    pub fn set_db_ok(&self, ok: bool) {
        self.db_ok.store(ok, Ordering::SeqCst);
    }

    /// 已失去租約（行程即將結束）
    pub fn lease_lost(&self) -> bool {
        self.lease_lost.load(Ordering::SeqCst)
    }

    /// 記下「已失去租約」；回傳是否為第一次
    pub fn mark_lease_lost(&self) -> bool {
        !self.lease_lost.swap(true, Ordering::SeqCst)
    }

    pub fn cluster_id(&self) -> Option<String> {
        self.cluster_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_cluster_id(&self, id: Option<String>) {
        *self.cluster_id.lock().unwrap_or_else(|e| e.into_inner()) = id;
    }

    pub fn leader(&self) -> Option<LeaderInfo> {
        self.leader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 記下目前看到的 leader；持有者改變時計一次換手
    pub fn set_leader(&self, leader: Option<LeaderInfo>) {
        let mut cur = self.leader.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = &leader {
            self.epoch.store(l.epoch, Ordering::SeqCst);
            if cur.as_ref().is_some_and(|c| c.epoch != l.epoch) {
                self.leader_changes.fetch_add(1, Ordering::Relaxed);
            }
        }
        *cur = leader;
    }

    pub fn takeover(&self) -> Option<Takeover> {
        self.takeover
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_takeover(&self, t: Takeover) {
        *self.takeover.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
    }

    /// 最近一次讀到的節點清單（心跳時更新）
    pub fn nodes(&self) -> Vec<NodeRow> {
        self.nodes.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_nodes(&self, nodes: Vec<NodeRow>) {
        *self.nodes.lock().unwrap_or_else(|e| e.into_inner()) = nodes;
    }

    /// `/readyz` 的 `cluster` 狀態字
    pub fn check_word(&self) -> &'static str {
        if !self.db_ok() || self.lease_lost() {
            "lost"
        } else {
            self.role().as_check()
        }
    }

    pub fn schema_word(&self) -> &'static str {
        match self.schema() {
            SchemaCheck::Ok => "ok",
            SchemaCheck::Newer => "newer",
            SchemaCheck::Older => "older",
        }
    }

    pub fn keys_word(&self) -> &'static str {
        if self.keys_ok() { "ok" } else { "mismatch" }
    }

    /// 節點是否失聯（心跳超過 `node_lost_secs`，或已被 leader 標為 `lost`）
    pub fn node_is_lost(&self, n: &NodeRow) -> bool {
        n.role == store::role::LOST
            || (n.role != store::role::STOPPED
                && n.heartbeat_age_ms
                    > i64::try_from(self.settings.node_lost_after().as_millis())
                        .unwrap_or(i64::MAX))
    }

    /// 把叢集的指標寫進 `/metrics`（`kairos/HA.md` 第 14.4 節）
    pub fn render_metrics(&self, e: &mut kp_obs::prom::Exposition) {
        use kp_obs::prom::Kind;
        let role = self.role();
        e.family(
            "kairos_cluster_role",
            Kind::Gauge,
            "本節點在叢集中的角色（1 表示目前的角色）",
        );
        for (name, r) in [
            ("leader", Role::Leader),
            ("standby", Role::Standby),
            ("draining", Role::Draining),
        ] {
            let on = role == r || (r == Role::Standby && role == Role::Starting);
            e.sample(
                "kairos_cluster_role",
                &[("role", name)],
                if on && !self.lease_lost() { 1.0 } else { 0.0 },
            );
        }
        #[allow(clippy::cast_precision_loss)]
        e.gauge(
            "kairos_cluster_lease_epoch",
            "目前看到的 leader epoch（每次換手加一）",
            self.epoch.load(Ordering::Relaxed) as f64,
        );
        e.counter_by(
            "kairos_cluster_lease_renewals_total",
            "續約或嘗試取得租約的次數（ok：成功；lost：租約已被其他節點取得或到期；error：資料庫錯誤）",
            "result",
            &[
                ("ok", self.renew_ok.load(Ordering::Relaxed)),
                ("lost", self.renew_lost.load(Ordering::Relaxed)),
                ("error", self.renew_error.load(Ordering::Relaxed)),
            ],
        );
        e.counter(
            "kairos_cluster_leader_changes_total",
            "本節點看到的 leader 換手次數",
            self.leader_changes.load(Ordering::Relaxed),
        );
        let nodes = self.nodes();
        let lost = nodes.iter().filter(|n| self.node_is_lost(n)).count();
        let alive = nodes
            .iter()
            .filter(|n| n.role != store::role::STOPPED && !self.node_is_lost(n))
            .count();
        e.family("kairos_cluster_nodes", Kind::Gauge, "叢集節點數，依狀態");
        #[allow(clippy::cast_precision_loss)]
        for (state, n) in [("alive", alive), ("lost", lost)] {
            e.sample("kairos_cluster_nodes", &[("state", state)], n as f64);
        }
        e.gauge(
            "kairos_cluster_keys_ok",
            "本節點的金鑰與叢集一致（1 是；0 時不參與 leader 選舉，KRS-E8504）",
            if self.keys_ok() { 1.0 } else { 0.0 },
        );
    }
}

static GLOBAL: OnceLock<Arc<ClusterState>> = OnceLock::new();

/// 這個行程的叢集狀態（沒有啟用叢集模式時為 `None`）
pub fn global() -> Option<Arc<ClusterState>> {
    GLOBAL.get().cloned()
}

/// 設定這個行程的叢集狀態（只在啟動時一次）
pub fn set_global(state: Arc<ClusterState>) {
    let _ = GLOBAL.set(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ClusterState {
        ClusterState::new("kairos-a".into(), ClusterSettings::default(), "h".into())
    }

    #[test]
    fn readiness_words_follow_the_role_and_database_state() {
        let s = state();
        // 還沒有連上資料庫
        assert_eq!(s.check_word(), "lost");
        s.set_db_ok(true);
        assert_eq!(s.check_word(), "standby");
        s.set_role(Role::Leader);
        assert_eq!(s.check_word(), "leader");
        assert!(s.is_leader());
        // 失去租約之後不再是 leader
        assert!(s.mark_lease_lost());
        assert!(!s.mark_lease_lost());
        assert_eq!(s.check_word(), "lost");
        assert!(!s.is_leader());
        assert_eq!(s.keys_word(), "mismatch");
        s.set_keys(Vec::new());
        assert_eq!(s.keys_word(), "ok");
        s.set_schema(SchemaCheck::Newer);
        assert_eq!(s.schema_word(), "newer");
    }

    #[test]
    fn leader_changes_are_counted_by_epoch() {
        let s = state();
        let l = |node: &str, epoch| {
            Some(LeaderInfo {
                node_id: node.into(),
                epoch,
                since: 0,
            })
        };
        s.set_leader(l("kairos-a", 1));
        s.set_leader(l("kairos-a", 1));
        assert_eq!(s.leader_changes.load(Ordering::Relaxed), 0);
        s.set_leader(l("kairos-b", 2));
        assert_eq!(s.leader_changes.load(Ordering::Relaxed), 1);
        assert_eq!(s.epoch.load(Ordering::Relaxed), 2);
        // 暫時讀不到租約不算換手
        s.set_leader(None);
        s.set_leader(l("kairos-b", 2));
        assert_eq!(s.leader_changes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn metrics_list_the_role_and_node_counts() {
        let s = state();
        s.set_db_ok(true);
        s.set_role(Role::Leader);
        let node = |id: &str, role: &str, age| NodeRow {
            node_id: id.into(),
            hostname: id.into(),
            version: "v".into(),
            schema_version: 10,
            role: role.into(),
            config_hash: "h".into(),
            keys_hash: "k".into(),
            advertise: None,
            started_at: 0,
            heartbeat_at: 0,
            info: None,
            heartbeat_age_ms: age,
        };
        s.set_nodes(vec![
            node("kairos-a", "leader", 100),
            node("kairos-b", "standby", 30_000),
            node("kairos-c", "stopped", 900_000),
        ]);
        let mut e = kp_obs::prom::Exposition::new();
        s.render_metrics(&mut e);
        let text = e.finish();
        assert!(
            text.contains("kairos_cluster_role{role=\"leader\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("kairos_cluster_role{role=\"standby\"} 0"),
            "{text}"
        );
        assert!(
            text.contains("kairos_cluster_nodes{state=\"alive\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("kairos_cluster_nodes{state=\"lost\"} 1"),
            "{text}"
        );
    }
}
