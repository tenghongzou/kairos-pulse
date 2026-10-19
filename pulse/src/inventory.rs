//! 主機資產（feature `inventory`）的收集與送出規則（METRICS.md 第 6.3 節）。
//!
//! - 收集：背景的 [`run_worker`] 在連線要求時（連線建立、之後每 `inventory.interval`）於 blocking
//!   執行緒收集一次（逾時 10 秒，記 `PLS-W8005`、這次不送），結果以 `watch` 發布。沒有任何連線協商了
//!   `inventory` 時不收集。
//! - 送出（[`InventoryConn`]，每條連線一個）：`hello_ack` 協商了 `inventory` 後要求收集，收到結果就送
//!   （不論與上次是否相同）；之後摘要（`inventory_digest`）與「上次收到 `ack` 的摘要」不同才送，距離
//!   上次收到 `ack` 滿 24 小時也送。`invalid_report` 後同一個摘要不再送（等內容改變）；
//!   `storage_unavailable` 5 分鐘後重送（不超過 `inventory.interval`）。斷線時在途的視同沒有送出。
//! - `inventory` 不經過本機緩衝（它是狀態，不是時序；重連後一定會再送一次）。
//!
//! 「上次收到 ack 的摘要」與「被拒絕的摘要」跨連線保留在 [`InventoryAgent`]（記憶體中；
//! 重新啟動 Pulse 後第一次連線本來就會送）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kairos_proto::{
    ClientMsg, Inventory, InventoryReport, ServerMsg, inventory_digest, parse_server_msg,
    validate_inventory,
};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::codes;
use crate::log_throttle::LogThrottle;
use crate::system_info::inventory::InventoryOptions;

/// 距離上次送達滿這麼久就再送一次（METRICS.md 第 6.3 節第 2 點）
pub const FORCE_RESEND_AFTER: Duration = Duration::from_secs(24 * 3600);
/// `storage_unavailable` 後重送的延遲（不超過 `inventory.interval`）
pub const RETRY_AFTER_STORAGE: Duration = Duration::from_secs(300);
/// 收集的逾時（第 6.3 節第 5 點）
pub const COLLECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 送出後多久沒有回應就視為結果不明（伺服器最多 8 秒內回應；斷線另由讀取閒置判斷）
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// 一次收集的結果
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// 收集時間（`inventory` 訊息的 `ts`）
    pub ts: String,
    pub inventory: Inventory,
    pub digest: String,
}

/// 收集函式（測試時替換）：回傳資產與取不到的項目
pub type CollectFn = fn(&InventoryOptions) -> (Inventory, Vec<&'static str>);

/// 跨連線保留的送出紀錄
#[derive(Debug, Default)]
struct Memory {
    /// 上次收到 ack 的摘要與時間
    acked: Option<(String, Instant)>,
    /// 被 `invalid_report` 拒絕的摘要（內容改變前不再送）
    rejected: Option<String>,
}

impl Memory {
    /// 這個摘要是否該送出。`force`：這條連線還沒有送達過（連線建立後的第一次，不論內容是否相同）
    fn should_send(&self, digest: &str, now: Instant, force: bool) -> bool {
        if self.rejected.as_deref() == Some(digest) {
            return false;
        }
        if force {
            return true;
        }
        match &self.acked {
            None => true,
            Some((d, at)) => {
                d != digest || now.saturating_duration_since(*at) >= FORCE_RESEND_AFTER
            }
        }
    }
}

/// 資產的收集與跨連線的送出紀錄（`inventory.enabled = true` 時建立一個）
pub struct InventoryAgent {
    /// `inventory.interval`
    pub interval: Duration,
    opts: InventoryOptions,
    collect: CollectFn,
    request: Notify,
    latest: watch::Sender<Option<Arc<Snapshot>>>,
    memory: Mutex<Memory>,
}

impl std::fmt::Debug for InventoryAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InventoryAgent")
            .field("interval", &self.interval)
            .field("opts", &self.opts)
            .finish_non_exhaustive()
    }
}

impl InventoryAgent {
    pub fn new(interval: Duration, opts: InventoryOptions) -> Arc<Self> {
        Self::with_collector(interval, opts, crate::system_info::inventory::collect)
    }

    pub fn with_collector(
        interval: Duration,
        opts: InventoryOptions,
        collect: CollectFn,
    ) -> Arc<Self> {
        Arc::new(Self {
            interval,
            opts,
            collect,
            request: Notify::new(),
            latest: watch::channel(None).0,
            memory: Mutex::new(Memory::default()),
        })
    }

    fn memory(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 重送前的延遲：5 分鐘，不超過 `inventory.interval`
    fn retry_delay(&self) -> Duration {
        RETRY_AFTER_STORAGE.min(self.interval)
    }
}

/// 背景收集：每次被要求時收集一次並發布。上一次的收集卡住（逾時後仍未完成）時不再發起新的收集
pub async fn run_worker(agent: Arc<InventoryAgent>) {
    let mut stuck: Option<tokio::task::JoinHandle<(Inventory, Vec<&'static str>)>> = None;
    let mut timeouts = LogThrottle::new(Duration::from_secs(3600));
    loop {
        agent.request.notified().await;
        if let Some(task) = &stuck {
            if !task.is_finished() {
                continue;
            }
            stuck = None;
        }
        let collect = agent.collect;
        let opts = agent.opts.clone();
        let mut task = tokio::task::spawn_blocking(move || collect(&opts));
        let (inventory, limited) = match tokio::time::timeout(COLLECT_TIMEOUT, &mut task).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Ok(Err(_)) => return,
            Err(_) => {
                if let Some(n) = timeouts.check(Instant::now()) {
                    log::warn!(
                        "{}收集主機資產超過 {} 秒仍未完成，這次不送 inventory（不影響 report 的取樣）{}",
                        codes::tag(codes::SAMPLE_TIMEOUT, Some(codes::W_SAMPLE_TIMEOUT)),
                        COLLECT_TIMEOUT.as_secs(),
                        crate::log_throttle::suppressed_note(n, Duration::from_secs(3600))
                    );
                }
                stuck = Some(task);
                continue;
            }
        };
        for item in limited {
            crate::system_info::report_limited(item, limited_detail(item));
        }
        if let Err(e) = validate_inventory(&inventory) {
            // 收集時已整理過每個欄位，正常不會發生
            log::debug!("收集到的主機資產不符合協定限制，這次不送：{e}");
            continue;
        }
        let snapshot = Snapshot {
            ts: crate::protocol::now_ts(),
            digest: inventory_digest(&inventory),
            inventory,
        };
        log::debug!("已收集主機資產（{}）", snapshot.digest);
        agent.latest.send_replace(Some(Arc::new(snapshot)));
    }
}

fn limited_detail(item: &str) -> &'static str {
    match item {
        "system.serial" if cfg!(target_os = "linux") => {
            "/sys/class/dmi/id/product_serial 只有 root 能讀，且沒有 /run/pulse/product_serial \
             （發佈包的 pulse.service 以 ExecStartPre=+ 複製；舊的 unit 請更新，見 INSTALL.md）"
        }
        "system.serial" => "SMBIOS 中沒有系統序號（或為廠商的預設字串）",
        _ => "權限或平台限制",
    }
}

/// 在途的 `inventory`
#[derive(Debug, Clone)]
struct InFlight {
    seq: u64,
    digest: String,
    sent_at: Instant,
}

/// 一則伺服器回應對 `inventory` 的處理結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryReply {
    /// 已保存
    Acked,
    /// 內容不合法（`invalid_report` 等）：同一個摘要不再送
    Rejected { code: String, message: String },
    /// 暫時無法保存：稍後重送
    Retry { message: String },
}

/// 一條連線上的 `inventory` 送出狀態
pub struct InventoryConn {
    agent: Arc<InventoryAgent>,
    rx: watch::Receiver<Option<Arc<Snapshot>>>,
    /// 下一次要求收集的時間
    next_collect: Instant,
    /// 這條連線還沒有送達過（第一次不論內容是否相同都送）
    force: bool,
    /// 已收到這條連線要求的收集結果（連線建立前的舊結果不送，等新的）
    fresh: bool,
    in_flight: Option<InFlight>,
    /// `storage_unavailable` 等之後的重送時間
    retry_at: Option<Instant>,
    /// 伺服器回應的日誌節流（跨連線保留在呼叫端也可以，這裡每條連線一個即可：每小時最多幾則）
    warn_log: LogThrottle,
}

impl InventoryConn {
    /// `hello_ack` 協商了 `inventory` 時建立：立即要求收集一次
    pub fn start(agent: Arc<InventoryAgent>, now: Instant) -> Self {
        let mut rx = agent.latest.subscribe();
        rx.borrow_and_update();
        Self {
            agent,
            rx,
            next_collect: now,
            force: true,
            fresh: false,
            in_flight: None,
            retry_at: None,
            warn_log: LogThrottle::new(Duration::from_secs(600)),
        }
    }

    /// 下一次需要處理計時的時間（要求收集、重送、24 小時、在途逾時）
    pub fn next_wake(&self) -> Instant {
        let mut t = self.next_collect;
        if let Some(r) = self.retry_at {
            t = t.min(r);
        }
        if let Some(f) = &self.in_flight {
            t = t.min(f.sent_at + REPLY_TIMEOUT);
        }
        if let Some((_, at)) = &self.agent.memory().acked {
            t = t.min(*at + FORCE_RESEND_AFTER);
        }
        t
    }

    /// 等收集的結果（`select!` 的分支）
    pub async fn changed(&mut self) {
        if self.rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// 計時到了：要求收集、處理在途逾時與重送；回傳現在要送的資產（沒有時 `None`）
    pub fn on_wake(&mut self, now: Instant) -> Option<Arc<Snapshot>> {
        if now >= self.next_collect {
            self.agent.request.notify_one();
            self.next_collect = now + self.agent.interval;
        }
        if let Some(f) = &self.in_flight
            && now >= f.sent_at + REPLY_TIMEOUT
        {
            log::debug!("inventory（seq={}）沒有收到回應，稍後重送", f.seq);
            self.in_flight = None;
            self.retry_at = Some(now + self.agent.retry_delay());
        }
        if self.retry_at.is_some_and(|r| now >= r) {
            self.retry_at = None;
        }
        self.candidate(now)
    }

    /// 收到新的收集結果：回傳現在要送的資產（沒有時 `None`）
    pub fn on_snapshot(&mut self, now: Instant) -> Option<Arc<Snapshot>> {
        self.rx.borrow_and_update();
        self.fresh = true;
        self.candidate(now)
    }

    fn candidate(&self, now: Instant) -> Option<Arc<Snapshot>> {
        if !self.fresh || self.in_flight.is_some() || self.retry_at.is_some() {
            return None;
        }
        let snap = self.rx.borrow().clone()?;
        self.agent
            .memory()
            .should_send(&snap.digest, now, self.force)
            .then_some(snap)
    }

    /// 組出 `inventory` 訊息
    pub fn message(seq: u64, snap: &Snapshot) -> ClientMsg {
        ClientMsg::Inventory(Box::new(InventoryReport {
            seq,
            ts: snap.ts.clone(),
            inventory: snap.inventory.clone(),
        }))
    }

    /// 已送出
    pub fn sent(&mut self, seq: u64, snap: &Snapshot, now: Instant) {
        self.in_flight = Some(InFlight {
            seq,
            digest: snap.digest.clone(),
            sent_at: now,
        });
    }

    /// 這一行是不是在途 `inventory` 的回應（ack 或非 fatal 的 error，seq 相同）；是的話處理並回傳結果
    /// （呼叫端不再交給一般的處理）。回應到更後面的訊息而沒有收到它的回應時（例如 `seq` 為 `null` 的
    /// `bad_request`），視為結果不明、稍後重送
    pub fn on_line(
        &mut self,
        line: &str,
        answered: Option<u64>,
        now: Instant,
    ) -> Option<InventoryReply> {
        let f = self.in_flight.clone()?;
        let reply = match parse_server_msg(line).ok()? {
            ServerMsg::Ack { seq, .. } if seq == f.seq => InventoryReply::Acked,
            ServerMsg::Error {
                seq: Some(seq),
                code,
                message,
                fatal: false,
                ..
            } if seq == f.seq => {
                if code == "storage_unavailable" {
                    InventoryReply::Retry { message }
                } else {
                    InventoryReply::Rejected { code, message }
                }
            }
            _ => {
                if answered.is_some_and(|a| a > f.seq) {
                    self.in_flight = None;
                    self.retry_at = Some(now + self.agent.retry_delay());
                }
                return None;
            }
        };
        self.in_flight = None;
        let mut memory = self.agent.memory();
        match &reply {
            InventoryReply::Acked => {
                memory.acked = Some((f.digest.clone(), now));
                memory.rejected = None;
                self.force = false;
                log::debug!("inventory（seq={}）已保存（{}）", f.seq, f.digest);
            }
            InventoryReply::Rejected { code, message } => {
                memory.rejected = Some(f.digest.clone());
                self.force = false;
                if let Some(n) = self.warn_log.check(now) {
                    log::warn!(
                        "{}Kairos 拒收這台主機的資產資訊（seq={}，{}）：{}；內容改變前不再重送{}",
                        codes::tag(codes::INVENTORY_REJECTED, Some(codes::W_INVENTORY_REJECTED)),
                        f.seq,
                        crate::network::sanitize_for_log(code, 64),
                        crate::network::sanitize_for_log(message, 200),
                        crate::log_throttle::suppressed_note(n, Duration::from_secs(600))
                    );
                }
            }
            InventoryReply::Retry { message } if message.contains("rate limit") => {
                // 資產的速率限制（每條連線 60 秒一則）：正常的 Pulse 不會發生，只記 DEBUG、不對應任何代碼
                // （設計包 §30.3「不分配代碼的情況」；不可當成 PLS-W4101）
                let delay = self.agent.retry_delay();
                self.retry_at = Some(now + delay);
                log::debug!(
                    "inventory（seq={}）被 Kairos 限速（{}）；{} 秒後重送",
                    f.seq,
                    crate::network::sanitize_for_log(message, 200),
                    delay.as_secs()
                );
            }
            InventoryReply::Retry { message } => {
                let delay = self.agent.retry_delay();
                self.retry_at = Some(now + delay);
                let code = codes::W_SERVER_STORAGE;
                if let Some(n) = self.warn_log.check(now) {
                    log::warn!(
                        "{}Kairos 暫時無法保存這台主機的資產資訊（seq={}，storage_unavailable）：{}；{} 秒後重送{}",
                        codes::tag(codes::INVENTORY_REJECTED, Some(code)),
                        f.seq,
                        crate::network::sanitize_for_log(message, 200),
                        delay.as_secs(),
                        crate::log_throttle::suppressed_note(n, Duration::from_secs(600))
                    );
                }
            }
        }
        Some(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kairos_proto::{MemoryInventory, OsInventory};

    fn opts() -> InventoryOptions {
        InventoryOptions {
            hardware_serials: true,
            network_addresses: true,
            interface_exclude: Vec::new(),
        }
    }

    fn snap(memory: u64) -> Arc<Snapshot> {
        let inventory = Inventory {
            os: Some(OsInventory {
                family: Some("linux".into()),
                ..Default::default()
            }),
            memory: Some(MemoryInventory {
                total: Some(memory),
                swap_total: None,
            }),
            ..Default::default()
        };
        Arc::new(Snapshot {
            ts: "2026-10-20T02:00:00.120Z".into(),
            digest: inventory_digest(&inventory),
            inventory,
        })
    }

    fn agent() -> Arc<InventoryAgent> {
        InventoryAgent::with_collector(Duration::from_secs(3600), opts(), |_| {
            (Inventory::default(), Vec::new())
        })
    }

    fn publish(agent: &InventoryAgent, s: Arc<Snapshot>) {
        agent.latest.send_replace(Some(s));
    }

    fn ack(seq: u64) -> String {
        format!(r#"{{"type":"ack","seq":{seq}}}"#)
    }

    fn error(seq: u64, code: &str) -> String {
        format!(r#"{{"type":"error","seq":{seq},"code":"{code}","message":"m","fatal":false}}"#)
    }

    #[tokio::test(start_paused = true)]
    async fn digest_decides_whether_to_send_with_a_24_hour_refresh() {
        let agent = agent();
        let t0 = Instant::now();
        let mut conn = InventoryConn::start(agent.clone(), t0);
        // 連線建立：立即要求收集，結果到了就送（第一次不論內容）
        assert_eq!(conn.next_wake(), t0);
        assert!(conn.on_wake(t0).is_none(), "還沒有收集結果");
        publish(&agent, snap(8));
        let s = conn.on_snapshot(t0).expect("連線後第一次一定送");
        conn.sent(2, &s, t0);
        assert!(conn.on_snapshot(t0).is_none(), "在途時不重複送");
        assert_eq!(
            conn.on_line(&ack(2), Some(2), t0),
            Some(InventoryReply::Acked)
        );

        // 內容相同：不送
        let t1 = t0 + Duration::from_secs(3600);
        assert!(conn.on_wake(t1).is_none());
        publish(&agent, snap(8));
        assert!(conn.on_snapshot(t1).is_none());
        // 內容改變（記憶體從 8 變 16）：送
        let t2 = t1 + Duration::from_secs(3600);
        publish(&agent, snap(16));
        let s = conn.on_snapshot(t2).expect("內容改變要送");
        conn.sent(3, &s, t2);
        conn.on_line(&ack(3), Some(3), t2).unwrap();
        // 內容不變，距離上次送達滿 24 小時：送
        let t3 = t2 + FORCE_RESEND_AFTER - Duration::from_secs(1);
        publish(&agent, snap(16));
        assert!(conn.on_snapshot(t3).is_none());
        let s = conn
            .on_wake(t2 + FORCE_RESEND_AFTER)
            .expect("滿 24 小時要送");
        assert_eq!(s.digest, snap(16).digest);

        // 新的連線：不論內容是否相同，第一次一定送
        let mut conn = InventoryConn::start(agent.clone(), t3);
        assert!(
            conn.on_wake(t3).is_none(),
            "連線建立前的舊結果不送，等這條連線要求的新收集"
        );
        publish(&agent, snap(16));
        assert!(conn.on_snapshot(t3).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_report_is_not_resent_until_the_content_changes() {
        let agent = agent();
        let t0 = Instant::now();
        let mut conn = InventoryConn::start(agent.clone(), t0);
        conn.on_wake(t0);
        publish(&agent, snap(8));
        let s = conn.on_snapshot(t0).unwrap();
        conn.sent(2, &s, t0);
        let ((), logs) = crate::test_log::capture(|| {
            let r = conn.on_line(&error(2, "invalid_report"), Some(2), t0);
            assert!(matches!(r, Some(InventoryReply::Rejected { .. })), "{r:?}");
        });
        assert!(
            logs.iter()
                .any(|(l, m)| *l == log::Level::Warn
                    && m.starts_with("inventory.rejected PLS-W2108 ")),
            "{logs:?}"
        );
        // 同一個摘要：之後、重連後都不再送
        publish(&agent, snap(8));
        assert!(conn.on_snapshot(t0 + Duration::from_secs(10)).is_none());
        assert!(conn.on_wake(t0 + FORCE_RESEND_AFTER * 2).is_none());
        let mut again = InventoryConn::start(agent.clone(), t0);
        publish(&agent, snap(8));
        assert!(again.on_snapshot(t0).is_none(), "被拒絕的內容重連後也不送");
        // 內容改變：送
        publish(&agent, snap(16));
        assert!(again.on_snapshot(t0).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn storage_unavailable_is_retried_after_five_minutes() {
        let agent = agent();
        let t0 = Instant::now();
        let mut conn = InventoryConn::start(agent.clone(), t0);
        conn.on_wake(t0);
        publish(&agent, snap(8));
        let s = conn.on_snapshot(t0).unwrap();
        conn.sent(2, &s, t0);
        let r = conn.on_line(&error(2, "storage_unavailable"), Some(2), t0);
        assert!(matches!(r, Some(InventoryReply::Retry { .. })));
        assert_eq!(conn.next_wake(), t0 + RETRY_AFTER_STORAGE);
        assert!(conn.on_wake(t0 + Duration::from_secs(299)).is_none());
        let s = conn
            .on_wake(t0 + RETRY_AFTER_STORAGE)
            .expect("5 分鐘後重送（仍是這條連線的第一次送達）");
        conn.sent(5, &s, t0 + RETRY_AFTER_STORAGE);
        // 其他訊息的回應不屬於 inventory
        assert_eq!(conn.on_line(&ack(4), Some(4), t0), None);
        // 回應到更後面的訊息卻沒有 inventory 的回應（例如 seq 為 null 的 bad_request）：稍後重送
        assert_eq!(
            conn.on_line(&ack(6), Some(6), t0 + RETRY_AFTER_STORAGE),
            None
        );
        assert!(conn.in_flight.is_none() && conn.retry_at.is_some());
        // 間隔較短的 inventory.interval：重送延遲不超過它
        let short = InventoryAgent::with_collector(Duration::from_secs(120), opts(), |_| {
            (Inventory::default(), Vec::new())
        });
        assert_eq!(short.retry_delay(), Duration::from_secs(120));
    }

    /// 資產的速率限制（`inventory rate limit exceeded`）：同樣稍後重送，但只記 DEBUG，不消耗 WARN 的節流
    /// （設計包 §30.3：不對應 `PLS-W4101` 或任何代碼）
    #[tokio::test(start_paused = true)]
    async fn inventory_rate_limit_is_retried_quietly() {
        let agent = agent();
        let t0 = Instant::now();
        let mut conn = InventoryConn::start(agent.clone(), t0);
        conn.on_wake(t0);
        publish(&agent, snap(8));
        let s = conn.on_snapshot(t0).unwrap();
        conn.sent(2, &s, t0);
        let line = r#"{"type":"error","seq":2,"code":"storage_unavailable","message":"inventory rate limit exceeded","fatal":false}"#;
        let r = conn.on_line(line, Some(2), t0);
        assert!(matches!(r, Some(InventoryReply::Retry { .. })));
        assert_eq!(conn.next_wake(), t0 + RETRY_AFTER_STORAGE);
        assert!(
            conn.warn_log.check(t0).is_some(),
            "速率限制不應記 WARN（節流仍未使用）"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn collection_is_requested_at_connect_and_every_interval() {
        let agent = agent();
        let t0 = Instant::now();
        let mut conn = InventoryConn::start(agent.clone(), t0);
        assert_eq!(conn.next_wake(), t0);
        conn.on_wake(t0);
        assert_eq!(conn.next_wake(), t0 + Duration::from_secs(3600));
        // 在途太久沒有回應：視為結果不明，稍後重送
        publish(&agent, snap(8));
        let s = conn.on_snapshot(t0).unwrap();
        conn.sent(2, &s, t0);
        assert_eq!(conn.next_wake(), t0 + REPLY_TIMEOUT);
        assert!(conn.on_wake(t0 + REPLY_TIMEOUT).is_none());
        assert!(conn.retry_at.is_some());
    }

    #[tokio::test]
    async fn worker_collects_on_request_and_publishes_a_valid_snapshot() {
        fn fake(_: &InventoryOptions) -> (Inventory, Vec<&'static str>) {
            (
                Inventory {
                    boot_time: Some("2026-10-02T08:11:04Z".into()),
                    ..Default::default()
                },
                vec!["system.serial"],
            )
        }
        let agent = InventoryAgent::with_collector(Duration::from_secs(3600), opts(), fake);
        let worker = tokio::spawn(run_worker(agent.clone()));
        let mut conn = InventoryConn::start(agent.clone(), Instant::now());
        conn.on_wake(Instant::now());
        tokio::time::timeout(Duration::from_secs(5), conn.changed())
            .await
            .unwrap();
        let s = conn.on_snapshot(Instant::now()).unwrap();
        assert_eq!(
            s.inventory.boot_time.as_deref(),
            Some("2026-10-02T08:11:04Z")
        );
        let msg = serde_json::to_value(InventoryConn::message(7, &s)).unwrap();
        assert_eq!(msg["type"], "inventory");
        assert_eq!(msg["seq"], 7);
        kairos_proto::validate_client_msg(
            &kairos_proto::parse_client_msg(&msg.to_string()).unwrap(),
        )
        .unwrap();
        worker.abort();
    }
}
