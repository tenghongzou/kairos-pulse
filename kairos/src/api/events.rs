//! 即時變更推送（SSE，`kairos/API.md` 第 6 節）。
//!
//! - [`pump`]：訂閱狀態表的變化（`Registry::subscribe`），轉成 `host.added`／`host.online`／
//!   `host.offline` 事件，編上 `<epoch>-<序號>` 交給 [`EventHub`]。
//! - [`EventHub`]：保留最近 [`RING_CAPACITY`] 則事件（`Last-Event-ID` 重連時補送）並廣播給所有串流。
//! - [`stream`]：一條 SSE 連線；先送 `ready`（必要時 `resync`），之後即時推送，每
//!   [`HEARTBEAT`] 一行註解，送出端跟不上時改送 `resync`。

use chrono::Utc;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Bytes;
use log::debug;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, broadcast, mpsc};
use tokio::time::{MissedTickBehavior, interval_at};

use super::ApiContext;
use super::auth::Principal;
use super::http::{ApiBody, ApiError, event_stream, ts};
use crate::registry::{HostChange, HostChangeKind};
use crate::shutdown::Shutdown;

/// 記憶體中保留的事件數（`Last-Event-ID` 重連時可補送的範圍）
pub const RING_CAPACITY: usize = 4096;
/// 每條串流在伺服器端的緩衝（則）；滿時改送 `resync`
pub const STREAM_BUFFER: usize = 256;
/// 心跳間隔
pub const HEARTBEAT: Duration = Duration::from_secs(15);
/// 用戶端的重連間隔（`retry:`）
pub const RETRY_MS: u64 = 5000;

/// 一則事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub seq: u64,
    pub kind: &'static str,
    /// JSON
    pub data: String,
}

/// 廣播的內容
#[derive(Debug, Clone)]
pub enum HubMsg {
    Event(Arc<StreamEvent>),
    /// 要求所有串流的用戶端重新抓取列表（事件可能遺漏）
    Resync(&'static str),
}

/// 事件的保留與廣播
#[derive(Debug)]
pub struct EventHub {
    epoch: String,
    last: AtomicU64,
    ring: Mutex<VecDeque<Arc<StreamEvent>>>,
    tx: broadcast::Sender<HubMsg>,
}

/// 從 `Last-Event-ID` 接續的結果
#[derive(Debug)]
pub enum Resume {
    /// 沒有 `Last-Event-ID`：從現在開始
    Fresh,
    /// 補送這些事件後接續
    Resumed(Vec<Arc<StreamEvent>>),
    /// 無法接續，送出 `resync`（原因）
    Resync(&'static str),
}

impl EventHub {
    /// `epoch`：這次啟動的識別碼（啟動時間的 base36）
    pub fn new(started_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            epoch: base36(u64::try_from(started_ms).unwrap_or(0)),
            last: AtomicU64::new(0),
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            tx: broadcast::channel(STREAM_BUFFER * 4).0,
        })
    }

    pub fn id(&self, seq: u64) -> String {
        format!("{}-{}", self.epoch, seq)
    }

    /// 目前最新的事件編號（還沒有事件時序號為 0）
    pub fn last_id(&self) -> String {
        self.id(self.last.load(Ordering::SeqCst))
    }

    /// 送出一則事件（在保留區的鎖內編號與廣播，兩者的順序一致）
    pub fn publish(&self, kind: &'static str, data: Value) -> u64 {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let seq = self.last.load(Ordering::SeqCst) + 1;
        let event = Arc::new(StreamEvent {
            seq,
            kind,
            data: data.to_string(),
        });
        if ring.len() >= RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(event.clone());
        self.last.store(seq, Ordering::SeqCst);
        let _ = self.tx.send(HubMsg::Event(event));
        seq
    }

    /// 要求所有串流重新同步
    pub fn resync_all(&self, reason: &'static str) {
        let _ = self.tx.send(HubMsg::Resync(reason));
    }

    /// 訂閱，並依 `Last-Event-ID` 決定補送的事件。訂閱與讀取保留區在同一把鎖內，不會漏掉中間的事件
    pub fn subscribe(
        &self,
        last_event_id: Option<&str>,
    ) -> (broadcast::Receiver<HubMsg>, Resume, u64) {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let rx = self.tx.subscribe();
        let last = self.last.load(Ordering::SeqCst);
        let resume = match last_event_id.map(str::trim).filter(|s| !s.is_empty()) {
            None => Resume::Fresh,
            Some(id) => match id.rsplit_once('-') {
                Some((epoch, seq)) if epoch == self.epoch => match seq.parse::<u64>() {
                    Ok(after) if after <= last => {
                        let oldest = ring.front().map_or(last + 1, |e| e.seq);
                        if after + 1 >= oldest {
                            Resume::Resumed(
                                ring.iter().filter(|e| e.seq > after).cloned().collect(),
                            )
                        } else {
                            Resume::Resync("too_old")
                        }
                    }
                    _ => Resume::Resync("too_old"),
                },
                _ => Resume::Resync("restarted"),
            },
        };
        (rx, resume, last)
    }
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// 一則狀態變化轉成事件的種類與內容；`is_new` 為清冊中沒有的主機第一次上線
pub fn event_of(change: &HostChange, is_new: bool) -> (&'static str, Value) {
    let ip = change
        .peer
        .parse::<std::net::SocketAddr>()
        .map(|a| a.ip().to_canonical().to_string())
        .ok();
    let mut data = json!({
        "agent_id": &*change.agent_id,
        "hostname": &*change.hostname,
        "display_name": change.display_name,
        "at": ts(change.at),
        "ip": ip,
    });
    let kind = match change.kind {
        HostChangeKind::Online => {
            data["status"] = json!("online");
            if is_new { "host.added" } else { "host.online" }
        }
        HostChangeKind::Offline(reason) => {
            data["status"] = json!("offline");
            data["reason"] = json!(reason.as_str());
            "host.offline"
        }
    };
    (kind, data)
}

/// 告警的變化 → 事件（直到關機）：`alert.created`、`alert.updated`、`alert.resolved`，`data` 為事件的摘要
/// （`kairos/API.md` 第 12.6 節）。落後時送出 `resync`
pub async fn alert_pump(
    mut rx: broadcast::Receiver<crate::alerts::engine::Change>,
    hub: Arc<EventHub>,
    shutdown: Arc<Shutdown>,
) {
    loop {
        let change = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            c = rx.recv() => c,
        };
        match change {
            Ok(c) => {
                hub.publish(c.kind, c.event);
            }
            Err(broadcast::error::RecvError::Lagged(_)) => hub.resync_all("overflow"),
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// 狀態表的變化 → 事件（直到關機）。`mark_known` 回傳這台主機是否第一次出現（`host.added`）
pub async fn pump(
    mut rx: broadcast::Receiver<HostChange>,
    hub: Arc<EventHub>,
    mark_known: impl Fn(&str) -> bool + Send + 'static,
    shutdown: Arc<Shutdown>,
) {
    loop {
        let change = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            r = rx.recv() => r,
        };
        match change {
            Ok(change) => {
                let is_new =
                    matches!(change.kind, HostChangeKind::Online) && mark_known(&change.agent_id);
                let (kind, data) = event_of(&change, is_new);
                hub.publish(kind, data);
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                debug!("主控台事件：狀態變化來不及處理，略過 {n} 則，要求串流重新同步");
                hub.resync_all("overflow");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

fn sse_event(id: Option<&str>, kind: &str, data: &str) -> Bytes {
    let mut s = String::with_capacity(data.len() + 48);
    if let Some(id) = id {
        s.push_str("id: ");
        s.push_str(id);
        s.push('\n');
    }
    s.push_str("event: ");
    s.push_str(kind);
    s.push_str("\ndata: ");
    s.push_str(data);
    s.push_str("\n\n");
    Bytes::from(s)
}

fn resync_event(reason: &str) -> Bytes {
    sse_event(
        None,
        "resync",
        &json!({ "reason": reason, "server_time": ts(Utc::now()) }).to_string(),
    )
}

/// `GET /api/v1/events`
pub async fn stream(
    ctx: Arc<ApiContext>,
    principal: Principal,
    last_event_id: Option<String>,
) -> Result<Response<ApiBody>, ApiError> {
    let permit: OwnedSemaphorePermit = ctx.streams.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "too_many_streams",
            format!(
                "即時推送的同時連線數已達上限 {}（console.max_streams）",
                ctx.max_streams
            ),
        )
        .with_retry_after(10)
    })?;
    let (mut rx, resume, last) = ctx.events.subscribe(last_event_id.as_deref());
    let (tx, body) = mpsc::channel::<Bytes>(STREAM_BUFFER);
    let resumed = matches!(resume, Resume::Resumed(_));
    let ready = json!({
        "server_time": ts(Utc::now()),
        "last_event_id": ctx.events.id(last),
        "resumed": resumed,
    });
    // 開頭幾則一定放得下（緩衝是空的）
    let _ = tx.try_send(Bytes::from(format!("retry: {RETRY_MS}\n\n")));
    let _ = tx.try_send(sse_event(None, "ready", &ready.to_string()));
    let mut backlog = Vec::new();
    match resume {
        Resume::Fresh => {}
        Resume::Resync(reason) => {
            let _ = tx.try_send(resync_event(reason));
        }
        Resume::Resumed(events) => backlog = events,
    }
    let hub = ctx.events.clone();
    let shutdown = ctx.shutdown.clone();
    let token_id = principal.token_id().to_string();
    let ctx_for_auth = ctx.clone();
    tokio::spawn(async move {
        let mut last_sent = last;
        let mut overflow = false;
        // 補送的事件可能比緩衝多：逐則等待送出
        for ev in backlog {
            if tx
                .send(sse_event(Some(&hub.id(ev.seq)), ev.kind, &ev.data))
                .await
                .is_err()
            {
                return;
            }
            last_sent = last_sent.max(ev.seq);
        }
        let mut heartbeat = interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let pending: Option<Bytes> = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                _ = tx.closed() => return,
                msg = rx.recv() => match msg {
                    Ok(HubMsg::Event(ev)) if ev.seq > last_sent => {
                        last_sent = ev.seq;
                        Some(sse_event(Some(&hub.id(ev.seq)), ev.kind, &ev.data))
                    }
                    Ok(HubMsg::Event(_)) => None,
                    Ok(HubMsg::Resync(reason)) => Some(resync_event(reason)),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        overflow = true;
                        None
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = heartbeat.tick() => {
                    let now = Utc::now();
                    if !ctx_for_auth.auth.still_valid(&token_id, now).await {
                        debug!("主控台事件：token {token_id} 已撤銷或過期，關閉串流");
                        return;
                    }
                    Some(Bytes::from(format!(": heartbeat {}\n\n", ts(now))))
                }
            };
            // 之前來不及送出：先要求重新同步
            if overflow && tx.capacity() > 0 {
                if tx.try_send(resync_event("overflow")).is_err() {
                    return;
                }
                overflow = false;
            }
            if let Some(bytes) = pending {
                match tx.try_send(bytes) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => overflow = true,
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        }
    });
    Ok(event_stream(ApiBody::Stream {
        rx: body,
        _permit: Some(permit),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::OfflineReason;

    #[test]
    fn last_event_id_resumes_or_requests_a_resync() {
        let hub = EventHub::new(1_760_335_407_123);
        assert!(hub.last_id().ends_with("-0"));
        let a = hub.publish("host.online", json!({"agent_id": "a"}));
        let b = hub.publish("host.offline", json!({"agent_id": "a"}));
        assert_eq!((a, b), (1, 2));
        let (_, r, last) = hub.subscribe(None);
        assert!(matches!(r, Resume::Fresh));
        assert_eq!(last, 2);
        let (_, r, _) = hub.subscribe(Some(&hub.id(1)));
        let Resume::Resumed(events) = r else {
            panic!("{r:?}")
        };
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [2]);
        // 最新的：沒有要補送的
        let (_, r, _) = hub.subscribe(Some(&hub.id(2)));
        assert!(matches!(r, Resume::Resumed(ref v) if v.is_empty()));
        // 另一次啟動的編號、未來的編號、格式錯誤
        let (_, r, _) = hub.subscribe(Some("zzz-1"));
        assert!(matches!(r, Resume::Resync("restarted")));
        let (_, r, _) = hub.subscribe(Some(&hub.id(99)));
        assert!(matches!(r, Resume::Resync("too_old")));
        let (_, r, _) = hub.subscribe(Some("garbage"));
        assert!(matches!(r, Resume::Resync("restarted")));
        // 超過保留範圍
        for _ in 0..RING_CAPACITY {
            hub.publish("host.online", json!({}));
        }
        let (_, r, _) = hub.subscribe(Some(&hub.id(1)));
        assert!(matches!(r, Resume::Resync("too_old")));
        let oldest = hub.ring.lock().unwrap().front().unwrap().seq;
        let (_, r, _) = hub.subscribe(Some(&hub.id(oldest - 1)));
        assert!(matches!(r, Resume::Resumed(ref v) if v.len() == RING_CAPACITY));
    }

    #[test]
    fn host_changes_become_events() {
        let change = HostChange {
            agent_id: "6f1c0e4a-1b2c-4d3e-9f00-112233445566".into(),
            hostname: "hmi-07".into(),
            display_name: None,
            peer: "10.30.3.27:51544".into(),
            kind: HostChangeKind::Offline(OfflineReason::Timeout),
            at: chrono::DateTime::from_timestamp_millis(1_760_335_407_123).unwrap(),
        };
        let (kind, data) = event_of(&change, false);
        assert_eq!(kind, "host.offline");
        assert_eq!(data["reason"], "timeout");
        assert_eq!(data["status"], "offline");
        assert_eq!(data["ip"], "10.30.3.27");
        assert_eq!(data["at"], "2025-10-13T06:03:27.123Z");
        let online = HostChange {
            kind: HostChangeKind::Online,
            ..change
        };
        assert_eq!(event_of(&online, true).0, "host.added");
        assert_eq!(event_of(&online, false).0, "host.online");
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        let bytes = sse_event(Some("x-1"), "host.online", "{}");
        assert_eq!(&bytes[..], b"id: x-1\nevent: host.online\ndata: {}\n\n");
    }
}
