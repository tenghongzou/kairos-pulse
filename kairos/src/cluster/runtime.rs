//! 叢集協調的執行緒（`kairos/HA.md` 第 6.1、6.2 節）：節點註冊與心跳、取得與續約 leader 租約、
//! 金鑰與設定的檢查、節點失聯的處理、正常關機時的交還。
//!
//! 在自己的 OS 執行緒（`kairos-cluster`）上以一條**專用連線**執行（同步驅動；不佔連線池、不做 fencing）。
//! 另有一條看門狗執行緒（`kairos-lease-watchdog`）只看單調時鐘：即使協調執行緒卡在沒有回應的資料庫
//! 呼叫上，leader 也會在 `lease_ttl_secs − 5` 秒沒有續約成功時結束。
//!
//! 正確性只靠資料庫的租約列與 fencing；`LISTEN/NOTIFY`（[`NOTIFY_CHANNEL`]）只讓備援在 leader 交還租約時
//! 不必等下一次輪詢。

use log::{debug, error, info, warn};
use std::io::{Error, ErrorKind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::keys::LocalKeys;
use super::lease::{LeaseKeeper, Renew};
use super::store::{ClusterStore, LEASE_LEADER, NodeRegistration, role as db_role};
use super::{
    CODE_CONFIG_DRIFT, CODE_COORDINATION, CODE_KEYS, CODE_LEADER_ACQUIRED, CODE_LEASE_LOST,
    CODE_NODE_LOST, ClusterState, LeaderInfo, META_CLUSTER_ID, META_CLUSTER_KEYS, NOTIFY_CHANNEL,
    Role, SchemaCheck, Takeover,
};
use crate::config::cluster::ACQUIRE_INTERVAL;
use crate::pki::store::PkiStore;
use crate::settings::SettingsStore;
use crate::store::postgres::conn::PgConn;
use crate::store::postgres::pool::{self, Fence, PgPool, ServiceMode};

/// 協調執行緒每一輪最多等待的時間（也是回應關機與看門狗的粒度）
const TICK: Duration = Duration::from_millis(250);
/// 金鑰檢查的間隔：不一致時、一致之後
const KEYS_RECHECK_FAILED: Duration = Duration::from_secs(5);
const KEYS_RECHECK_OK: Duration = Duration::from_secs(60);
/// 同一種狀況重複記錄日誌的最短間隔
const LOG_EVERY: Duration = Duration::from_secs(60);
/// 沒有任何節點持有租約超過這個時間記 `KRS-E8503`
const NO_LEADER_ALERT: Duration = Duration::from_secs(60);
/// 續約失敗（資料庫錯誤）後的重試間隔
const RENEW_RETRY: Duration = Duration::from_secs(1);

/// 取得租約的結果（交給 `main::run`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub epoch: i64,
    /// 上一個持有者（第一次建立租約時為 `None`）
    pub previous: Option<String>,
}

/// 失去租約時的處理（正式程式：記錄後以結束代碼 75 結束行程；測試可替換）
pub type LostAction = Arc<dyn Fn(String) + Send + Sync>;

/// 正式的 [`LostAction`]：記錄 `KRS-W8502` 後立即以結束代碼 75 結束行程
pub fn exit_on_lost(node_id: String) -> LostAction {
    Arc::new(move |reason: String| {
        crate::shutdown::log_and_exit(
            i32::from(super::EXIT_LEASE_LOST),
            format!(
                "警告 {CODE_LEASE_LOST}：本節點（{node_id}）已失去 leader 租約（{reason}）；立即停止服務並以結束代碼 {} 結束，由服務管理員重新啟動為備援",
                super::EXIT_LEASE_LOST
            ),
        );
    })
}

/// 看門狗與協調執行緒共用的「最後一次續約成功」時間
#[derive(Debug)]
struct LeaseClock {
    base: Instant,
    /// 最晚要續約成功的時間（距離 `base` 的毫秒數）；0＝沒有持有租約
    deadline_ms: AtomicU64,
}

impl LeaseClock {
    fn set_deadline(&self, at: Instant) {
        let ms = u64::try_from(at.saturating_duration_since(self.base).as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        self.deadline_ms.store(ms, Ordering::SeqCst);
    }

    fn clear(&self) {
        self.deadline_ms.store(0, Ordering::SeqCst);
    }

    fn overdue(&self, now: Instant) -> bool {
        let d = self.deadline_ms.load(Ordering::SeqCst);
        d != 0 && now.saturating_duration_since(self.base) > Duration::from_millis(d)
    }
}

/// 啟動協調所需的參數
pub struct StartParams {
    pub state: Arc<ClusterState>,
    pub pool: Arc<PgPool>,
    pub data_dir: PathBuf,
    pub hostname: String,
    pub shutdown: Arc<crate::shutdown::Shutdown>,
    pub on_lost: LostAction,
}

/// `main::run` 持有的控制端
pub struct ClusterHandle {
    pub state: Arc<ClusterState>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    grant: Option<tokio::sync::oneshot::Receiver<Result<Grant, Error>>>,
}

impl ClusterHandle {
    /// 等到本節點取得 leader 租約。啟動檢查失敗（不符合要求的資料庫、schema 較新、`node_id` 重複……）
    /// 時回錯誤；在取得之前就被要求停止（關機）時回 `Ok(None)`
    pub async fn wait_leader(&mut self) -> Result<Option<Grant>, Error> {
        match self.grant.take() {
            Some(rx) => match rx.await {
                Ok(Ok(g)) => Ok(Some(g)),
                Ok(Err(e)) => Err(e),
                Err(_) => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// 正常停止：交還租約（若持有）、把節點標為 `stopped`，等協調執行緒結束（最多約 3 秒）
    pub async fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                tokio::task::spawn_blocking(move || {
                    let _ = t.join();
                }),
            )
            .await;
        }
    }
}

impl Drop for ClusterHandle {
    /// 沒有經過 [`ClusterHandle::stop`] 就被丟棄（啟動途中失敗）：仍要求協調執行緒交還租約
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// 啟動協調執行緒與看門狗
pub fn start(p: StartParams) -> Result<ClusterHandle, Error> {
    p.pool.set_service_mode(Some(ServiceMode::Cluster));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let clock = Arc::new(LeaseClock {
        base: Instant::now(),
        deadline_ms: AtomicU64::new(0),
    });
    let state = p.state.clone();

    // fencing 檢查失敗（任何寫入發現租約已不屬於自己）
    {
        let state = state.clone();
        let on_lost = p.on_lost.clone();
        p.pool.on_fence_lost(move || {
            lost(
                &state,
                &on_lost,
                "寫入資料庫時發現租約已到期或已被其他節點取得",
            );
        });
    }

    // 看門狗：只看單調時鐘
    {
        let clock = clock.clone();
        let state = state.clone();
        let on_lost = p.on_lost.clone();
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("kairos-lease-watchdog".into())
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if clock.overdue(Instant::now()) {
                        lost(
                            &state,
                            &on_lost,
                            &format!(
                                "超過 {:.0} 秒沒有續約成功（cluster.lease_ttl_secs − 5 秒）",
                                state.settings.self_demote_after().as_secs_f64()
                            ),
                        );
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })?;
    }

    let mut coordinator = Coordinator {
        state: state.clone(),
        pool: p.pool,
        data_dir: p.data_dir,
        hostname: p.hostname,
        shutdown: p.shutdown,
        on_lost: p.on_lost,
        stop: stop.clone(),
        clock,
        grant: Some(tx),
        conn: None,
        started_at: None,
        keeper: None,
        draining: false,
        notified: false,
        next_heartbeat: None,
        next_keys: None,
        next_attempt: None,
        next_renew: None,
        no_leader_since: None,
        logged: Mutex::new(Vec::new()),
        drift_logged: None,
        local_keys_hash: String::new(),
    };
    let thread = std::thread::Builder::new()
        .name("kairos-cluster".into())
        .spawn(move || coordinator.run())?;
    Ok(ClusterHandle {
        state,
        stop,
        thread: Some(thread),
        grant: Some(rx),
    })
}

/// 失去租約：只處理第一次；正在正常關機（角色已是 `stopped`）時不處理
fn lost(state: &ClusterState, on_lost: &LostAction, reason: &str) {
    if state.role() == Role::Stopped {
        return;
    }
    if state.mark_lease_lost() {
        state.renew_lost.fetch_add(1, Ordering::Relaxed);
        on_lost(reason.to_string());
    }
}

/// 啟動時不能繼續的錯誤（行程以設定錯誤結束）
struct Fatal(Error);

struct Coordinator {
    state: Arc<ClusterState>,
    pool: Arc<PgPool>,
    data_dir: PathBuf,
    hostname: String,
    shutdown: Arc<crate::shutdown::Shutdown>,
    on_lost: LostAction,
    stop: Arc<AtomicBool>,
    clock: Arc<LeaseClock>,
    grant: Option<tokio::sync::oneshot::Sender<Result<Grant, Error>>>,
    conn: Option<PgConn>,
    /// 這次啟動在 `cluster_nodes` 的 `started_at`（已註冊時）
    started_at: Option<i64>,
    keeper: Option<LeaseKeeper>,
    draining: bool,
    /// 收到 NOTIFY：下一輪立即嘗試取得租約
    notified: bool,
    next_heartbeat: Option<Instant>,
    next_keys: Option<Instant>,
    next_attempt: Option<Instant>,
    next_renew: Option<Instant>,
    no_leader_since: Option<Instant>,
    /// 節流：（種類, 上次記錄的時間）
    logged: Mutex<Vec<(&'static str, Instant)>>,
    /// 上次記錄設定不一致時 leader 的 config_hash
    drift_logged: Option<String>,
    local_keys_hash: String,
}

fn due(at: Option<Instant>, now: Instant) -> bool {
    at.is_none_or(|t| now >= t)
}

impl Coordinator {
    fn should_log(&self, kind: &'static str) -> bool {
        let mut logged = self.logged.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        match logged.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, at)) if now.duration_since(*at) < LOG_EVERY => false,
            Some((_, at)) => {
                *at = now;
                true
            }
            None => {
                logged.push((kind, now));
                true
            }
        }
    }

    fn run(&mut self) {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                self.stop_clean();
                return;
            }
            if let Err(Fatal(e)) = self.step() {
                // 啟動檢查失敗：交給 main（以設定錯誤結束）；已是 leader 時不會走到這裡
                match self.grant.take() {
                    Some(tx) => {
                        let _ = tx.send(Err(e));
                    }
                    None => error!("叢集協調無法繼續: {e}"),
                }
                self.state.set_role(Role::Stopped);
                self.conn = None;
                return;
            }
            self.wait();
        }
    }

    /// 等到下一件事到期（最多 [`TICK`]）；連線著的時候順便等 NOTIFY
    fn wait(&mut self) {
        let now = Instant::now();
        let next = [
            self.next_heartbeat,
            self.next_keys,
            self.next_attempt,
            self.next_renew,
        ]
        .into_iter()
        .flatten()
        .min()
        .map_or(TICK, |t| t.saturating_duration_since(now))
        .clamp(Duration::from_millis(10), TICK);
        match &self.conn {
            Some(conn) if !conn.is_broken() => match conn.wait_notification(next) {
                Ok(true) => self.notified = true,
                Ok(false) => {}
                Err(e) => {
                    debug!("叢集協調的連線中斷: {e}");
                    self.conn = None;
                }
            },
            _ => std::thread::sleep(next),
        }
    }

    /// 連線逾時：續約要在自我降級的時限內得到結果
    fn statement_timeout(&self) -> Duration {
        (self.state.settings.self_demote_after() / 2).min(Duration::from_secs(5))
    }

    fn connect(&mut self) -> Result<(), Fatal> {
        if self.conn.as_ref().is_some_and(|c| !c.is_broken()) {
            return Ok(());
        }
        self.conn = None;
        let conn = match self.pool.dedicated(pool::DEFAULT_BUSY) {
            Ok(c) => c,
            Err(e) => {
                self.state.set_db_ok(false);
                if self.should_log("db") {
                    error!(
                        event = "cluster.database_unreachable", code = pool::CODE_PG_UNREACHABLE;
                        "錯誤 {}：叢集協調無法連線到資料庫（{}）；持續重試: {e}",
                        pool::CODE_PG_UNREACHABLE,
                        self.pool.settings().display_target()
                    );
                }
                return Ok(());
            }
        };
        conn.exempt_from_fencing();
        let ms = self.statement_timeout().as_millis().max(500);
        let setup = conn
            .execute_batch(&format!(
                "SET statement_timeout = {ms}; SET lock_timeout = {ms}; SET application_name = 'kairos-cluster'"
            ))
            .and_then(|()| conn.listen(NOTIFY_CHANNEL));
        if let Err(e) = setup {
            self.state.set_db_ok(false);
            debug!("叢集協調的連線初始設定失敗: {e}");
            return Ok(());
        }
        // 啟動保護：叢集的節點以共享鎖標示「這個 schema 由叢集使用」
        if let Err(e) = self.pool.claim_service_mode(&conn) {
            if e.kind() == ErrorKind::ResourceBusy {
                return Err(Fatal(e));
            }
            self.state.set_db_ok(false);
            debug!("{e}");
            return Ok(());
        }
        self.conn = Some(conn);
        self.state.set_db_ok(true);
        Ok(())
    }

    /// 第一次連上資料庫：檢查伺服器、確認 schema、檢查 `node_id` 沒有重複、註冊節點
    fn register(&mut self) -> Result<(), Fatal> {
        let Some(conn) = self.conn.as_ref() else {
            return Ok(());
        };
        let transient = |what: &str, e: &dyn std::fmt::Display| {
            debug!("叢集協調：{what}: {e}");
        };
        let info = match pool::server_info(conn) {
            Ok(i) => i,
            Err(e) => {
                transient("無法讀取 PostgreSQL 的狀態", &e);
                self.conn = None;
                return Ok(());
            }
        };
        if let Some(why) = info.unmet_requirement() {
            return Err(Fatal(Error::new(
                ErrorKind::InvalidData,
                format!("{}：{why}", pool::CODE_PG_REQUIREMENTS),
            )));
        }
        self.pool.note_server(&info);
        if self
            .pool
            .warning_due(crate::store::postgres::SERVER_WARNING_INTERVAL)
        {
            crate::store::postgres::log_warnings(&crate::store::postgres::server_warnings(
                &self.pool, &info,
            ));
        }
        match crate::store::postgres::ensure_schema(conn) {
            Ok(_) => self.state.set_schema(SchemaCheck::Ok),
            Err(e) => {
                let text = e.to_string();
                if text.contains(pool::CODE_PG_SCHEMA_NEWER) {
                    self.state.set_schema(SchemaCheck::Newer);
                }
                if [
                    pool::CODE_PG_SCHEMA_NEWER,
                    pool::CODE_PG_REQUIREMENTS,
                    crate::store::postgres::CODE_MIGRATE_FAILED,
                ]
                .iter()
                .any(|c| text.contains(c))
                {
                    return Err(Fatal(e));
                }
                transient("無法確認 schema", &e);
                if conn.is_broken() {
                    self.conn = None;
                }
                return Ok(());
            }
        }
        let node_id = self.state.node_id.clone();
        // node_id 重複（kairos/HA.md 第 6.1 節）：同名節點的心跳還很新、而且在另一台主機上
        match conn.cluster_node(&node_id, None) {
            Ok(Some(existing)) => {
                let fresh_ms =
                    i64::try_from(3 * self.state.settings.heartbeat_interval().as_millis())
                        .unwrap_or(i64::MAX);
                let active = !matches!(existing.role.as_str(), db_role::STOPPED | db_role::LOST);
                if active
                    && existing.heartbeat_age_ms < fresh_ms
                    && existing.hostname != self.hostname
                {
                    return Err(Fatal(Error::new(
                        ErrorKind::AlreadyExists,
                        format!(
                            "{CODE_COORDINATION}：cluster.node_id「{node_id}」已被另一個執行中的節點使用（主機 {}，{} 毫秒前還有心跳）。每個節點的 node_id 必須不同；請修改這個節點的 cluster.node_id",
                            existing.hostname, existing.heartbeat_age_ms
                        ),
                    )));
                }
            }
            Ok(None) => {}
            Err(e) => {
                transient("無法讀取節點清單", &e);
                return Ok(());
            }
        }
        let local = LocalKeys::read(&self.data_dir);
        self.local_keys_hash = local.hash();
        let advertise = self.state.settings.advertise.trim();
        let reg = NodeRegistration {
            node_id: &node_id,
            hostname: &self.hostname,
            version: crate::VERSION,
            schema_version: i64::from(crate::store::sqlite::SCHEMA_VERSION),
            config_hash: &self.state.config_hash,
            keys_hash: &self.local_keys_hash,
            advertise: (!advertise.is_empty()).then_some(advertise),
            info: None,
        };
        match conn.cluster_node_register(&reg, None) {
            Ok(started_at) => {
                self.started_at = Some(started_at);
                info!(
                    event = "cluster.node_registered";
                    "叢集：節點 {node_id} 已註冊（主機 {}，設定雜湊 {}）",
                    self.hostname, self.state.config_hash
                );
            }
            Err(e) => transient("無法註冊節點", &e),
        }
        match conn.get_meta(META_CLUSTER_ID) {
            Ok(id) => self.state.set_cluster_id(id),
            Err(e) => transient("無法讀取 cluster_id", &e),
        }
        Ok(())
    }

    /// 金鑰檢查（`kairos/HA.md` 第 11.1 節）
    fn check_keys(&mut self, now: Instant) {
        let Some(conn) = self.conn.as_ref() else {
            return;
        };
        let (meta, cas) = match (conn.get_meta(META_CLUSTER_KEYS), conn.list_cas()) {
            (Ok(m), Ok(c)) => (m, c),
            (Err(e), _) | (_, Err(e)) => {
                debug!("叢集協調：無法讀取金鑰指紋: {e}");
                self.next_keys = Some(now + KEYS_RECHECK_FAILED);
                return;
            }
        };
        if self.state.cluster_id().is_none()
            && let Ok(id) = conn.get_meta(META_CLUSTER_ID)
        {
            self.state.set_cluster_id(id);
        }
        let local = LocalKeys::read(&self.data_dir);
        self.local_keys_hash = local.hash();
        let problems = local.compare(meta.as_deref(), &cas);
        let was_ok = self.state.keys_ok();
        if problems.is_empty() {
            if !was_ok {
                info!(
                    event = "cluster.keys_ok";
                    "叢集：本節點的金鑰與叢集一致（指紋雜湊 {}）",
                    self.local_keys_hash
                );
            }
            self.next_keys = Some(now + KEYS_RECHECK_OK);
        } else {
            if was_ok || self.should_log("keys") {
                error!(
                    event = "cluster.keys_mismatch", code = CODE_KEYS;
                    "錯誤 {CODE_KEYS}：本節點的金鑰與叢集不一致或缺少（{}）；不進入就緒、不參與 leader 選舉、不自動建立金鑰。請以 kairos cluster keys import 匯入叢集的金鑰（第一個節點尚未建立叢集時先執行 kairos cluster init）",
                    problems.join("；")
                );
            }
            self.next_keys = Some(now + KEYS_RECHECK_FAILED);
        }
        self.state.set_keys(problems);
    }

    fn node_info(&self) -> String {
        serde_json::json!({
            "pid": std::process::id(),
            "connections": self.state.connections(),
            "keys_ok": self.state.keys_ok(),
            "key_problems": self.state.key_problems(),
            "schema": self.state.schema_word(),
            "epoch": self.keeper.as_ref().map(LeaseKeeper::epoch),
        })
        .to_string()
    }

    /// 心跳，並更新節點清單與目前的 leader；leader 另外處理失聯的節點
    fn heartbeat(&mut self, now: Instant) -> Result<(), Fatal> {
        let (Some(conn), Some(started_at)) = (self.conn.as_ref(), self.started_at) else {
            return Ok(());
        };
        self.next_heartbeat = Some(now + self.state.settings.heartbeat_interval());
        let node_id = self.state.node_id.clone();
        let role = if self.keeper.is_some() {
            Role::Leader
        } else if self.draining {
            Role::Draining
        } else if self.state.keys_ok() {
            Role::Standby
        } else {
            Role::Starting
        };
        if self.keeper.is_none() {
            self.state.set_role(role);
        }
        let stored = match conn.cluster_node_heartbeat(
            &node_id,
            started_at,
            role.as_db(),
            &self.local_keys_hash,
            Some(&self.node_info()),
            None,
        ) {
            Ok(r) => r,
            Err(e) => {
                debug!("叢集協調：心跳失敗: {e}");
                if conn.is_broken() {
                    self.conn = None;
                }
                return Ok(());
            }
        };
        match stored.as_deref() {
            None => {
                // 紀錄不見了：被 kairos cluster forget 刪除，或另一個行程以同一個 node_id 重新註冊
                match conn.cluster_node(&node_id, None) {
                    Ok(None) => {
                        warn!(
                            "叢集：節點 {node_id} 的紀錄已被刪除（kairos cluster forget），重新註冊"
                        );
                        self.started_at = None;
                    }
                    Ok(Some(other)) => {
                        let msg = format!(
                            "{CODE_COORDINATION}：cluster.node_id「{node_id}」已被另一個行程重新註冊（主機 {}）；每個節點的 node_id 必須不同",
                            other.hostname
                        );
                        if self.keeper.is_some() {
                            lost(&self.state, &self.on_lost, &msg);
                        }
                        return Err(Fatal(Error::new(ErrorKind::AlreadyExists, msg)));
                    }
                    Err(_) => {}
                }
                return Ok(());
            }
            Some(db_role::DRAINING) if !self.draining => {
                self.draining = true;
                if self.keeper.is_some() {
                    info!(
                        event = "cluster.drain", code = "KRS-I8507";
                        "資訊 KRS-I8507：節點 {node_id} 被要求排空（kairos cluster drain）：本節點是 leader，開始正常關機並交還租約，由備援節點接手"
                    );
                    self.shutdown.trigger();
                } else {
                    self.state.set_role(Role::Draining);
                    info!(
                        event = "cluster.drain", code = "KRS-I8507";
                        "資訊 KRS-I8507：節點 {node_id} 被要求排空（kairos cluster drain）：不再參與 leader 選舉；重新啟動後恢復"
                    );
                }
            }
            Some(_) => {}
        }

        // 節點清單與目前的租約（系統健康、/metrics、設定一致性）
        if let Ok(lease) = conn.cluster_lease(LEASE_LEADER, None) {
            let live = lease.filter(super::store::LeaseRow::is_live);
            match &live {
                Some(l) => {
                    self.no_leader_since = None;
                    self.state.set_leader(Some(LeaderInfo {
                        node_id: l.holder.clone(),
                        epoch: l.epoch,
                        since: l.acquired_at,
                    }));
                }
                None => {
                    self.state.set_leader(None);
                    let since = *self.no_leader_since.get_or_insert(now);
                    if now.duration_since(since) > NO_LEADER_ALERT && self.should_log("no_leader") {
                        error!(
                            event = "cluster.no_leader", code = CODE_COORDINATION;
                            "錯誤 {CODE_COORDINATION}：叢集已超過 {} 秒沒有任何節點持有 leader 租約（告警、彙總、保留、CRL 暫停，agent 無法連線）；請確認各節點的 /readyz 與金鑰狀態",
                            now.duration_since(since).as_secs()
                        );
                    }
                }
            }
        }
        if let Ok(nodes) = conn.cluster_nodes(None) {
            if let Some(leader) = self.state.leader()
                && leader.node_id != node_id
                && let Some(l) = nodes.iter().find(|n| n.node_id == leader.node_id)
                && l.config_hash != self.state.config_hash
                && self.drift_logged.as_deref() != Some(l.config_hash.as_str())
            {
                self.drift_logged = Some(l.config_hash.clone());
                warn!(
                    event = "cluster.config_drift", code = CODE_CONFIG_DRIFT;
                    "警告 {CODE_CONFIG_DRIFT}：本節點需要一致的設定與 leader（{}）不同（本節點 {}，leader {}）；請比對兩個節點的 kairos config show --effective",
                    leader.node_id, self.state.config_hash, l.config_hash
                );
            }
            self.state.set_nodes(nodes);
        }
        if self.keeper.is_some() {
            let lost_after = i64::try_from(self.state.settings.node_lost_after().as_millis())
                .unwrap_or(i64::MAX);
            if let Ok(lost_nodes) = conn.cluster_mark_lost(lost_after, &node_id, None) {
                for n in lost_nodes {
                    warn!(
                        event = "cluster.node_lost", code = CODE_NODE_LOST;
                        "警告 {CODE_NODE_LOST}：叢集節點 {n} 失聯（超過 {} 秒沒有心跳）",
                        self.state.settings.node_lost_secs
                    );
                }
            }
        }
        Ok(())
    }

    /// 備援：嘗試取得租約
    fn try_acquire(&mut self, now: Instant) {
        let Some(conn) = self.conn.as_ref() else {
            return;
        };
        self.next_attempt = Some(now + ACQUIRE_INTERVAL);
        self.notified = false;
        let node_id = self.state.node_id.clone();
        let ttl = self.state.settings.lease_ttl();
        let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        // 上一個持有者（換手的判斷；讀不到不影響取得）
        let previous = conn
            .cluster_lease(LEASE_LEADER, None)
            .ok()
            .flatten()
            .map(|l| l.holder);
        let sent = Instant::now();
        match conn.cluster_lease_acquire(LEASE_LEADER, &node_id, ttl_ms, None) {
            Ok(Some(row)) => {
                self.state.renew_ok.fetch_add(1, Ordering::Relaxed);
                let keeper = LeaseKeeper::new(
                    node_id.clone(),
                    row.epoch,
                    ttl,
                    self.state.settings.self_demote_after(),
                    sent,
                );
                self.clock.set_deadline(keeper.deadline());
                // fencing 先於任何寫入：之後這個池的寫入都確認租約
                self.pool.set_fence(Some(Fence {
                    node: node_id.clone(),
                    epoch: row.epoch,
                    now: None,
                }));
                self.keeper = Some(keeper);
                self.next_renew = Some(sent + self.state.settings.renew_interval());
                self.state.set_role(Role::Leader);
                self.state.set_leader(Some(LeaderInfo {
                    node_id: node_id.clone(),
                    epoch: row.epoch,
                    since: row.acquired_at,
                }));
                let previous = previous.filter(|p| !p.is_empty());
                self.state.set_takeover(Takeover {
                    previous: previous.clone(),
                    epoch: row.epoch,
                });
                info!(
                    event = "cluster.leader_acquired", code = CODE_LEADER_ACQUIRED;
                    "資訊 {CODE_LEADER_ACQUIRED}：本節點（{node_id}）取得 leader 租約（epoch {}{}），開始載入狀態並接受連線",
                    row.epoch,
                    match &previous {
                        Some(p) if *p != node_id => format!("；上一個 leader 是 {p}"),
                        Some(_) => "；上一個 leader 也是本節點".to_string(),
                        None => String::new(),
                    }
                );
                // 立刻寫一次心跳，讓其他節點與 CLI 看到角色
                self.next_heartbeat = None;
                if let Some(tx) = self.grant.take() {
                    let _ = tx.send(Ok(Grant {
                        epoch: row.epoch,
                        previous,
                    }));
                }
            }
            Ok(None) => {}
            Err(e) => {
                self.state.renew_error.fetch_add(1, Ordering::Relaxed);
                debug!("叢集協調：嘗試取得租約失敗: {e}");
                if conn.is_broken() {
                    self.conn = None;
                }
            }
        }
    }

    /// leader：續約
    fn renew(&mut self, now: Instant) {
        let Some(keeper) = self.keeper.as_mut() else {
            return;
        };
        if keeper.overdue(now) {
            lost(
                &self.state,
                &self.on_lost,
                &format!(
                    "超過 {:.0} 秒沒有續約成功（cluster.lease_ttl_secs − 5 秒）",
                    self.state.settings.self_demote_after().as_secs_f64()
                ),
            );
            return;
        }
        if !due(self.next_renew, now) {
            return;
        }
        let Some(conn) = self.conn.as_ref() else {
            self.next_renew = Some(now + RENEW_RETRY);
            return;
        };
        let sent = Instant::now();
        match keeper.renew(conn, sent, None) {
            Renew::Renewed { expires_at } => {
                debug!("叢集：租約已續約到 {expires_at}");
                self.state.renew_ok.fetch_add(1, Ordering::Relaxed);
                self.clock.set_deadline(keeper.deadline());
                self.next_renew = Some(sent + self.state.settings.renew_interval());
            }
            Renew::Lost => {
                lost(
                    &self.state,
                    &self.on_lost,
                    "租約已被其他節點取得、已被交還（kairos cluster step-down）或已到期",
                );
                // 測試用的 on_lost 不會結束行程：不再續約
                self.keeper = None;
                self.clock.clear();
            }
            Renew::Error(e) => {
                self.state.renew_error.fetch_add(1, Ordering::Relaxed);
                if self.should_log("renew") {
                    warn!("叢集：續約失敗（{e}）；重試中，超過時限仍未成功時本節點會結束");
                }
                if conn.is_broken() {
                    self.conn = None;
                }
                self.next_renew = Some(now + RENEW_RETRY);
            }
        }
    }

    fn step(&mut self) -> Result<(), Fatal> {
        let now = Instant::now();
        // 續約最優先（連線中斷時也要檢查是否逾期）
        self.renew(now);
        if self.state.lease_lost() {
            return Ok(());
        }
        self.connect()?;
        if self.conn.is_none() {
            return Ok(());
        }
        if self.started_at.is_none() {
            self.register()?;
            if self.started_at.is_none() {
                return Ok(());
            }
            self.next_keys = None;
            self.next_heartbeat = None;
        }
        if self.keeper.is_some() {
            self.renew(Instant::now());
        }
        if due(self.next_keys, now) {
            self.check_keys(now);
        }
        if due(self.next_heartbeat, now) {
            self.heartbeat(now)?;
        }
        let eligible = self.keeper.is_none()
            && !self.draining
            && self.state.keys_ok()
            && self.state.schema() == SchemaCheck::Ok
            && self.shutdown.started_at().is_none();
        if eligible && (self.notified || due(self.next_attempt, now)) {
            self.try_acquire(now);
            if self.keeper.is_some() {
                let _ = self.heartbeat(Instant::now());
            }
        }
        Ok(())
    }

    /// 正常停止：交還租約、標為 stopped、通知其他節點
    fn stop_clean(&mut self) {
        // 之後的 fencing 失敗（交還租約後才到的寫入）不再觸發「失去租約」
        self.state.set_role(Role::Stopped);
        self.clock.clear();
        let _ = self.connect();
        let Some(conn) = self.conn.as_ref() else {
            return;
        };
        let node_id = self.state.node_id.clone();
        if let Some(keeper) = self.keeper.take() {
            match conn.cluster_lease_release(LEASE_LEADER, &node_id, keeper.epoch(), None) {
                Ok(true) => info!("叢集：已交還 leader 租約（epoch {}）", keeper.epoch()),
                Ok(false) => {}
                Err(e) => warn!("叢集：無法交還 leader 租約（{e}）；其他節點要等租約到期才能接手"),
            }
        }
        if let Some(started_at) = self.started_at {
            let _ = conn.cluster_node_heartbeat(
                &node_id,
                started_at,
                db_role::STOPPED,
                &self.local_keys_hash,
                Some(&self.node_info()),
                None,
            );
        }
        let _ = conn.notify(NOTIFY_CHANNEL);
        self.conn = None;
    }
}
