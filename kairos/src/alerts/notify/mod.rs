//! 通知的派送（`kairos/ALERTS.md` 第 7 節；設計包 `alerts-ux.md` §3.4、§5.4）。
//!
//! - **速率**：每個管道每 30 秒最多 1 則；期間累積的通知合併成一則「另有 N 則」。緊急的第一則通知
//!   不排隊、不合併（§3.4）。同一組（例如彙總事件恢復時脫離的子事件）一律合併。
//! - **重試與退避**：失敗時 30 秒、1 分、2 分、4 分、5 分……後重試；對方回 429／503 並帶 `Retry-After`
//!   時依它退避；超過 15 分鐘仍無法送出則標為失敗（記下錯誤代碼）。
//! - **管道失敗**：同一管道連續 3 次發送失敗 → 管道狀態為 `failed`，引擎開啟系統事件「通知管道失敗」
//!   （`KRS-E5107`，改經其他健康的管道送出）。之後要送到這個管道的通知暫緩（`held`），每 2 分鐘以最舊的
//!   一則試送；成功後管道恢復，暫緩中的通知不逐則補發，改送一則彙整（最多保留 24 小時）。
//! - 每個管道同一時間只有一個發送工作；發送在背景進行，結果回到派送迴圈後寫入資料庫。

#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
pub mod fake;
pub mod render;
pub mod transport;

use chrono::FixedOffset;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

use super::model::{Channel, ChannelKind, Severity};
use super::secrets::SecretBox;
use super::store::{self, Delivery};
use crate::enroll::db::EnrollDb;
use crate::shutdown::Shutdown;
use render::View;
use transport::Outcome;

/// 每個管道兩則通知之間至少相隔
pub const CHANNEL_MIN_INTERVAL_MS: i64 = 30_000;
/// 放棄重試的期限
pub const GIVE_UP_MS: i64 = 15 * 60_000;
/// 退避的上限
pub const MAX_BACKOFF_MS: i64 = 5 * 60_000;
/// 連續失敗幾次標為失敗（與規則 `sys.channel_failed` 的預設相同）
pub const FAILURES_TO_FAIL: u32 = 3;
/// 失敗的管道多久試送一次
pub const PROBE_INTERVAL_MS: i64 = 2 * 60_000;
/// 暫緩的通知最多保留
pub const HOLD_MAX_MS: i64 = 24 * 3_600_000;
/// 一則合併的通知最多包含幾則
pub const MAX_MERGE: usize = 50;

/// 第 n 次失敗後的退避
pub fn backoff_ms(attempts: u32) -> i64 {
    let n = attempts.saturating_sub(1).min(10);
    (30_000i64 << n).min(MAX_BACKOFF_MS)
}

/// 派送需要的設定
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    pub offset: FixedOffset,
    pub console_url: String,
}

/// 管道的顯示選項：`timezone`（預設 `alerts.timezone`）與 `show_ip`（Email 與 Kairos JSON 預設完整 IP，
/// Slack、Teams、LINE 格式預設只顯示網段）
pub fn view_of(channel: &Channel, cfg: &DispatchConfig) -> View {
    let offset = channel
        .config
        .get("timezone")
        .and_then(Value::as_str)
        .and_then(crate::config::alerts_parse_offset)
        .unwrap_or(cfg.offset);
    let format = channel
        .config
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("kairos");
    let default_ip = match (channel.kind, format) {
        (ChannelKind::Webhook, "slack" | "teams" | "line") => "subnet",
        _ => "full",
    };
    View {
        offset,
        show_ip: channel
            .config
            .get("show_ip")
            .and_then(Value::as_str)
            .unwrap_or(default_ip)
            .to_string(),
        channel_name: channel.name.clone(),
    }
}

/// 一個發送單位：一則訊息與它涵蓋的發送紀錄
struct Unit {
    ids: Vec<i64>,
    payload: Value,
    /// 管道失敗時的試送
    probe: bool,
}

/// 發送工作的結果
struct JobResult {
    channel_id: String,
    results: Vec<(Unit, Outcome)>,
}

/// 派送器
pub struct Dispatcher {
    db: Arc<EnrollDb>,
    secrets: Arc<SecretBox>,
    cfg: DispatchConfig,
    channels: Mutex<HashMap<String, Channel>>,
    incoming: Mutex<Vec<Delivery>>,
    wake: Notify,
    reload: std::sync::atomic::AtomicBool,
}

/// 派送的狀態：待送的通知、進行中的發送工作、各管道最近一次送出的時間
pub struct Pump {
    d: Arc<Dispatcher>,
    pub queue: Vec<Delivery>,
    in_flight: HashSet<String>,
    last_sent: HashMap<String, i64>,
    tx: mpsc::Sender<JobResult>,
    rx: mpsc::Receiver<JobResult>,
    clock: fn() -> i64,
}

impl Pump {
    /// 讀取資料庫中待送的通知
    pub async fn new(d: Arc<Dispatcher>, clock: fn() -> i64) -> Self {
        let queue = match d.db.read(|conn| Ok(store::pending_deliveries(conn)?)).await {
            Ok(q) => q,
            Err(e) => {
                log::warn!("告警：無法讀取待送的通知：{e}");
                Vec::new()
            }
        };
        let (tx, rx) = mpsc::channel::<JobResult>(64);
        Self {
            d,
            queue,
            in_flight: HashSet::new(),
            last_sent: HashMap::new(),
            tx,
            rx,
            clock,
        }
    }

    /// 一個發送工作的結果
    async fn on_result(&mut self, done: JobResult, now: i64) {
        self.in_flight.remove(&done.channel_id);
        let d = self.d.clone();
        d.finish(done, &mut self.queue, &mut self.last_sent, now)
            .await;
    }

    /// 等一個發送工作結束並處理（測試用；沒有進行中的工作時回 `false`）
    #[cfg(test)]
    pub async fn settle(&mut self, now: i64) -> bool {
        if self.in_flight.is_empty() {
            return false;
        }
        match tokio::time::timeout(Duration::from_secs(30), self.rx.recv()).await {
            Ok(Some(done)) => {
                self.on_result(done, now).await;
                true
            }
            _ => false,
        }
    }

    /// 一輪：重新讀取管道、取入新的通知、決定並啟動這一輪的發送
    pub async fn round(&mut self, now: i64) {
        let d = self.d.clone();
        if d.reload.swap(false, std::sync::atomic::Ordering::SeqCst) {
            d.load_channels().await;
        }
        self.queue.extend(
            d.incoming
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..),
        );
        let mut changed: Vec<Delivery> = Vec::new();
        let channels = d.channels.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // 管道已刪除或停用：放棄；暫緩超過 24 小時：失敗
        for item in self.queue.iter_mut() {
            let ch = channels.get(&item.channel_id);
            if ch.is_none_or(|c| !c.enabled) {
                item.state = "dropped".into();
                item.error = Some("管道已停用或已刪除".into());
                changed.push(item.clone());
            } else if item.state == "held" && now - item.created_at > HOLD_MAX_MS {
                item.state = "failed".into();
                item.error = Some("管道失敗期間暫緩超過 24 小時".into());
                item.error_code = Some("KRS-E5107".into());
                changed.push(item.clone());
            }
        }
        self.queue
            .retain(|x| x.state == "pending" || x.state == "held");
        let mut by_channel: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, x) in self.queue.iter().enumerate() {
            by_channel.entry(x.channel_id.clone()).or_default().push(i);
        }
        let clock = self.clock;
        for (cid, idx) in by_channel {
            if self.in_flight.contains(&cid) {
                continue;
            }
            let Some(ch) = channels.get(&cid).cloned() else {
                continue;
            };
            let units = d.plan(
                &ch,
                &mut self.queue,
                &idx,
                &self.last_sent,
                now,
                &mut changed,
            );
            if units.is_empty() {
                continue;
            }
            self.in_flight.insert(cid.clone());
            let tx = self.tx.clone();
            let view = view_of(&ch, &d.cfg);
            tokio::spawn(async move {
                let mut results = Vec::new();
                for u in units {
                    let id = format!("kairos-{}-{}", u.ids.first().copied().unwrap_or(0), clock());
                    let o = transport::send(&ch, &u.payload, &view, clock(), &id).await;
                    let failed = !o.ok;
                    results.push((u, o));
                    // 失敗時同一個工作中的其他單位不再試（避免連續逾時）
                    if failed {
                        break;
                    }
                }
                let _ = tx
                    .send(JobResult {
                        channel_id: ch.channel_id.clone(),
                        results,
                    })
                    .await;
            });
        }
        if !changed.is_empty() {
            d.persist(changed, None).await;
        }
    }
}

impl Dispatcher {
    pub fn new(db: Arc<EnrollDb>, secrets: Arc<SecretBox>, cfg: DispatchConfig) -> Arc<Self> {
        Arc::new(Self {
            db,
            secrets,
            cfg,
            channels: Mutex::new(HashMap::new()),
            incoming: Mutex::new(Vec::new()),
            wake: Notify::new(),
            reload: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// 目前的管道（含派送端的狀態；引擎用來判斷「通知管道失敗」與路由）
    pub fn channels(&self) -> Vec<Channel> {
        let map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<Channel> = map.values().cloned().collect();
        v.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
        v
    }

    /// 排入新的發送紀錄（已寫入資料庫）
    pub fn enqueue(&self, items: Vec<Delivery>) {
        if items.is_empty() {
            return;
        }
        self.incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(items);
        self.wake.notify_one();
    }

    /// 管道的設定已變更：下一輪重新讀取
    pub fn reload_channels(&self) {
        self.reload.store(true, std::sync::atomic::Ordering::SeqCst);
        self.wake.notify_one();
    }

    async fn load_channels(&self) {
        let secrets = self.secrets.clone();
        let loaded = self
            .db
            .read(move |conn| Ok(store::load_channels(conn, Some(secrets.as_ref()))?))
            .await;
        match loaded {
            Ok(list) => {
                let mut map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
                *map = list
                    .into_iter()
                    .map(|c| (c.channel_id.clone(), c))
                    .collect();
            }
            Err(e) => {
                log::warn!("告警：無法讀取通知管道（稍後重試）：{e}");
                self.reload.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// 派送迴圈（直到關機）
    pub async fn run(self: Arc<Self>, shutdown: Arc<Shutdown>, clock: fn() -> i64) {
        let mut pump = Pump::new(self.clone(), clock).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                Some(done) = pump.rx.recv() => {
                    pump.on_result(done, clock()).await;
                }
                _ = self.wake.notified() => {}
                _ = ticker.tick() => {}
            }
            pump.round(clock()).await;
        }
    }

    /// 決定這一輪要送的內容（並把失敗管道的通知改為暫緩）
    fn plan(
        &self,
        ch: &Channel,
        queue: &mut [Delivery],
        idx: &[usize],
        last_sent: &HashMap<String, i64>,
        now: i64,
        changed: &mut Vec<Delivery>,
    ) -> Vec<Unit> {
        let due: Vec<usize> = idx
            .iter()
            .copied()
            .filter(|i| queue[*i].next_attempt_at <= now)
            .collect();
        if due.is_empty() {
            return Vec::new();
        }
        if ch.failed() {
            for i in &due {
                let d = &mut queue[*i];
                if d.state != "held" {
                    d.state = "held".into();
                    changed.push(d.clone());
                }
            }
            // 試送最舊的一則
            if ch
                .last_attempt_at
                .is_none_or(|t| now - t >= PROBE_INTERVAL_MS)
                && let Some(first) = due.iter().min_by_key(|i| queue[**i].id)
            {
                let d = &queue[*first];
                return vec![Unit {
                    ids: vec![d.id],
                    payload: d.payload.clone(),
                    probe: true,
                }];
            }
            return Vec::new();
        }
        let mut units = Vec::new();
        // 緊急的第一則通知：不排隊、不合併
        for i in &due {
            let d = &queue[*i];
            if d.urgent && d.attempts == 0 {
                units.push(Unit {
                    ids: vec![d.id],
                    payload: d.payload.clone(),
                    probe: false,
                });
            }
        }
        let rate_ok = last_sent
            .get(&ch.channel_id)
            .is_none_or(|t| now - t >= CHANNEL_MIN_INTERVAL_MS);
        let rest: Vec<usize> = due
            .iter()
            .copied()
            .filter(|i| !(queue[*i].urgent && queue[*i].attempts == 0))
            .collect();
        if rate_ok && !rest.is_empty() {
            let mut rest = rest;
            rest.sort_by_key(|i| queue[*i].id);
            rest.truncate(MAX_MERGE);
            if rest.len() == 1 {
                let d = &queue[rest[0]];
                units.push(Unit {
                    ids: vec![d.id],
                    payload: d.payload.clone(),
                    probe: false,
                });
            } else {
                let items: Vec<Value> = rest.iter().map(|i| queue[*i].payload.clone()).collect();
                units.push(Unit {
                    ids: rest.iter().map(|i| queue[*i].id).collect(),
                    payload: json!({
                        "kind": "digest",
                        "items": items,
                        "now": now,
                        "link": self.cfg.console_url.trim().trim_end_matches('/'),
                    }),
                    probe: false,
                });
            }
        }
        units
    }

    /// 發送結果：更新發送紀錄與管道的狀態
    async fn finish(
        &self,
        done: JobResult,
        queue: &mut Vec<Delivery>,
        last_sent: &mut HashMap<String, i64>,
        now: i64,
    ) {
        let mut changed = Vec::new();
        let mut channel = {
            let map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
            match map.get(&done.channel_id) {
                Some(c) => c.clone(),
                None => return,
            }
        };
        let mut recovered = false;
        for (unit, o) in &done.results {
            channel.last_attempt_at = Some(now);
            let batch = (unit.ids.len() > 1).then(|| unit.ids[0]);
            if o.ok {
                recovered |= channel.failed();
                channel.state = "ok".into();
                channel.consecutive_failures = 0;
                channel.failed_since = None;
                channel.last_success_at = Some(now);
                channel.last_error = None;
                channel.last_error_code = None;
                last_sent.insert(channel.channel_id.clone(), now);
                for d in queue.iter_mut().filter(|d| unit.ids.contains(&d.id)) {
                    d.state = "sent".into();
                    d.attempts += 1;
                    d.sent_at = Some(now);
                    d.batch_id = batch;
                    d.response_code = o.response_code.map(i64::from);
                    d.error = None;
                    d.error_code = None;
                    changed.push(d.clone());
                }
            } else {
                channel.consecutive_failures += 1;
                channel.last_error = Some(o.message.clone());
                channel.last_error_code = o.code.map(str::to_string);
                if channel.consecutive_failures >= FAILURES_TO_FAIL && !channel.failed() {
                    channel.state = "failed".into();
                    channel.failed_since = Some(now);
                    log::warn!(
                        "錯誤 KRS-E5107：通知管道「{}」連續 {} 次發送失敗，改由其他管道通知（最後的錯誤：{} {}）",
                        channel.name.escape_debug(),
                        channel.consecutive_failures,
                        o.code.unwrap_or("-"),
                        o.message
                    );
                }
                for d in queue.iter_mut().filter(|d| unit.ids.contains(&d.id)) {
                    d.attempts += 1;
                    d.response_code = o.response_code.map(i64::from);
                    d.error = Some(o.message.clone());
                    d.error_code = o.code.map(str::to_string);
                    if unit.probe {
                        d.next_attempt_at = now + PROBE_INTERVAL_MS;
                    } else if now - d.created_at >= GIVE_UP_MS {
                        d.state = "failed".into();
                        log::warn!(
                            "通知未送出（已重試 {} 次、超過 15 分鐘）：管道「{}」、事件 {}：{} {}",
                            d.attempts,
                            channel.name.escape_debug(),
                            d.event_id.as_deref().unwrap_or("-"),
                            o.code.unwrap_or("-"),
                            o.message
                        );
                    } else {
                        let wait = backoff_ms(d.attempts)
                            .max(o.retry_after_secs.map_or(0, |s| s as i64 * 1000));
                        d.next_attempt_at = now + wait;
                    }
                    changed.push(d.clone());
                }
            }
        }
        // 管道恢復：暫緩中的通知改送一則彙整
        if recovered {
            let held: Vec<&Delivery> = queue
                .iter()
                .filter(|d| d.channel_id == channel.channel_id && d.state == "held")
                .collect();
            if !held.is_empty() {
                let events: Vec<Value> = held
                    .iter()
                    .filter_map(|d| {
                        d.payload.get("event").map(
                            |e| json!({ "event_id": e.get("event_id"), "title": e.get("title") }),
                        )
                    })
                    .collect();
                let digest =
                    json!({ "kind": "channel_recovered", "missed": held.len(), "events": events });
                let ids: Vec<i64> = held.iter().map(|d| d.id).collect();
                for d in queue.iter_mut().filter(|d| ids.contains(&d.id)) {
                    d.state = "merged".into();
                    d.batch_id = ids.first().copied();
                    changed.push(d.clone());
                }
                self.persist(Vec::new(), Some((channel.channel_id.clone(), digest, now)))
                    .await;
                log::info!(
                    "通知管道「{}」已恢復；失敗期間的 {} 則通知改送一則彙整",
                    channel.name.escape_debug(),
                    ids.len()
                );
            }
        }
        queue.retain(|d| d.state == "pending" || d.state == "held");
        {
            let mut map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = map.get_mut(&channel.channel_id) {
                c.state = channel.state.clone();
                c.consecutive_failures = channel.consecutive_failures;
                c.failed_since = channel.failed_since;
                c.last_attempt_at = channel.last_attempt_at;
                c.last_success_at = channel.last_success_at;
                c.last_error = channel.last_error.clone();
                c.last_error_code = channel.last_error_code.clone();
            }
        }
        let status = channel.clone();
        if let Err(e) = self
            .db
            .write(move |conn| Ok(store::update_channel_status(conn, &status)?))
            .await
        {
            log::warn!("告警：無法寫入通知管道的狀態：{e}");
        }
        self.persist(changed, None).await;
    }

    /// 寫入發送紀錄的變化；`extra` 為要另外排入的一則（管道恢復時的彙整）
    async fn persist(&self, changed: Vec<Delivery>, extra: Option<(String, Value, i64)>) {
        if changed.is_empty() && extra.is_none() {
            return;
        }
        let result = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                for d in &changed {
                    store::update_delivery(&tx, d)?;
                }
                let mut new_ids = Vec::new();
                if let Some((cid, payload, now)) = &extra {
                    new_ids = store::insert_deliveries(
                        &tx,
                        &[super::engine::NewDelivery {
                            event_id: None,
                            channel_id: cid.clone(),
                            kind: "channel_recovered",
                            urgent: false,
                            payload: payload.clone(),
                            group: None,
                            created_at: *now,
                        }],
                    )?;
                }
                tx.commit()?;
                Ok((new_ids, extra))
            })
            .await;
        match result {
            Ok((ids, Some((cid, payload, now)))) => {
                self.enqueue(
                    ids.into_iter()
                        .map(|id| Delivery {
                            id,
                            event_id: None,
                            channel_id: cid.clone(),
                            kind: "channel_recovered".into(),
                            state: "pending".into(),
                            urgent: false,
                            payload: payload.clone(),
                            attempts: 0,
                            created_at: now,
                            next_attempt_at: now,
                            sent_at: None,
                            batch_id: None,
                            response_code: None,
                            error_code: None,
                            error: None,
                        })
                        .collect(),
                );
            }
            Ok(_) => {}
            Err(e) => log::warn!("告警：無法寫入通知的發送紀錄：{e}"),
        }
    }
}

/// 測試通知（S11「寄送測試通知」）：以真實範本與假資料直接送出，不排入佇列
pub async fn send_test(
    channel: &Channel,
    severity: Severity,
    cfg: &DispatchConfig,
    now: i64,
) -> Outcome {
    let payload = render::test_payload(severity, &cfg.console_url, now);
    let view = view_of(channel, cfg);
    transport::send(channel, &payload, &view, now, &format!("kairos-test-{now}")).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(3), 120_000);
        assert_eq!(backoff_ms(4), 240_000);
        assert_eq!(backoff_ms(5), MAX_BACKOFF_MS);
        assert_eq!(backoff_ms(50), MAX_BACKOFF_MS);
    }
}
