//! 每條連線上遠端管理訊息的收送（PROTOCOL.md「遠端管理」）。由 `network` 的兩個連線迴圈（at-most-once 與
//! durable）呼叫：
//!
//! - [`MgmtConn::start`]：`hello_ack` 之後，依協商結果與 `pending` 決定要送的 `config_status`、`config_get`、
//!   `upgrade_get`，以及恢復中的下載。
//! - [`MgmtConn::pop`]：取出下一則要送的訊息（呼叫端配上 `seq` 後送出，durable 連線另記入在途清單）。
//! - [`MgmtConn::on_line`]：伺服器訊息若是我們請求的回應（`config`、`upgrade_offer`、`upgrade_chunk`、
//!   `ack`、`error`）就處理並回傳它回應的 `seq`；回應 `heartbeat` 的 `ack` 只取出 `pending`，不攔截。
//! - [`MgmtConn::wants_reconnect`]：套用了需要重新連線的鍵時，送出 `config_status` 後結束這條連線。
//!
//! Kairos 依序回應每一則訊息，因此「比某個請求更晚送出的訊息已有回應」代表該請求的回應遺失（例如
//! `seq: null` 的 `bad_request`）：設定的請求在下一條連線重送，升級的狀態計入失敗次數。

use std::collections::VecDeque;
use std::sync::Arc;

use kairos_proto::mgmt::{ConfigGet, MgmtSession, Pending, UpgradeChunkGet, UpgradeGet};
use kairos_proto::{ClientMsg, ServerMsg, parse_server_msg};
use tokio::time::Instant;

use super::RemoteMgmt;
use super::upgrade::{ConnState, Out};

/// 我們送出、尚未收到回應的請求
#[derive(Debug, Clone, PartialEq, Eq)]
enum Req {
    ConfigGet,
    ConfigStatus(String),
    Upgrade(Out),
}

/// 一條連線上的遠端管理狀態
#[derive(Debug)]
pub struct MgmtConn {
    mgmt: Arc<RemoteMgmt>,
    session: MgmtSession,
    upgrade: ConnState,
    /// 在途的請求（依 seq 遞增）
    outstanding: VecDeque<(u64, Req)>,
    send_status: bool,
    fetch: bool,
    /// 收到 `config_get` 的回應前不再送
    fetch_in_flight: bool,
    /// 送出 `config_status` 後結束連線（需要重新連線的鍵）
    reconnect_after_status: bool,
    reconnect_now: bool,
}

/// [`MgmtConn::on_line`] 的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handled {
    /// 回應的 seq
    pub answered: Option<u64>,
}

impl MgmtConn {
    /// `hello_ack` 之後
    pub fn start(mgmt: Arc<RemoteMgmt>, session: MgmtSession, pending: Option<&Pending>) -> Self {
        let reg = mgmt.config.on_registered(session.config, pending);
        let upgrade = mgmt.upgrade.on_registered(session.upgrade, pending);
        Self {
            mgmt,
            session,
            upgrade,
            outstanding: VecDeque::new(),
            send_status: reg.send_status,
            fetch: reg.fetch,
            fetch_in_flight: false,
            reconnect_after_status: reg.reconnect && reg.send_status,
            reconnect_now: reg.reconnect && !reg.send_status,
        }
    }

    /// 需要結束這條連線、重建取樣與連線（套用了需要重新連線的鍵）
    pub fn wants_reconnect(&self) -> bool {
        self.reconnect_now
    }

    /// 下一次需要 [`MgmtConn::pop`] 的時間；沒有時 `None`
    pub fn next_wake(&self) -> Option<Instant> {
        let now = Instant::now();
        let mut t: Option<Instant> = None;
        let mut at = |x: Instant| t = Some(t.map_or(x, |y| y.min(x)));
        if (self.send_status || self.fetch) && !self.fetch_in_flight && self.session.config {
            at(now);
        }
        if self.session.config
            && let Some(d) = self.mgmt.config.next_deadline()
        {
            at(d);
        }
        if let Some(u) = self.mgmt.upgrade.next_wake(&self.upgrade, now) {
            at(u);
        }
        t
    }

    /// 取出下一則要送的訊息（`seq` 為呼叫端將使用的 seq）。另外執行定時的檢查（確認期、升級時段）
    pub fn pop(&mut self, seq: u64) -> Option<ClientMsg> {
        let now = Instant::now();
        if self.session.config {
            self.mgmt.config.on_tick(now);
            if self.send_status {
                self.send_status = false;
                let status = self.mgmt.config.status_message(seq);
                self.outstanding
                    .push_back((seq, Req::ConfigStatus(status.state.clone())));
                return Some(ClientMsg::ConfigStatus(Box::new(status)));
            }
            if self.fetch && !self.fetch_in_flight {
                self.fetch = false;
                self.fetch_in_flight = true;
                self.outstanding.push_back((seq, Req::ConfigGet));
                return Some(ClientMsg::ConfigGet(Box::new(ConfigGet {
                    seq,
                    ts: crate::protocol::now_ts(),
                    have_rev: self.mgmt.config.applied_rev(),
                })));
            }
        }
        if self.session.upgrade {
            let confirming = self.mgmt.config.confirming();
            self.mgmt.upgrade.tick(&mut self.upgrade, now, confirming);
            if let Some(out) = self.mgmt.upgrade.poll(&mut self.upgrade, now) {
                let msg = match &out {
                    Out::Get(id) => ClientMsg::UpgradeGet(Box::new(UpgradeGet {
                        seq,
                        ts: crate::protocol::now_ts(),
                        upgrade_id: id.clone(),
                    })),
                    Out::Chunk {
                        upgrade_id,
                        filename,
                        offset,
                        length,
                    } => ClientMsg::UpgradeChunkGet(Box::new(UpgradeChunkGet {
                        seq,
                        ts: crate::protocol::now_ts(),
                        upgrade_id: upgrade_id.clone(),
                        filename: filename.clone(),
                        offset: *offset,
                        length: *length,
                    })),
                    Out::Status(r) => ClientMsg::UpgradeStatus(Box::new(r.message(seq))),
                };
                self.outstanding.push_back((seq, Req::Upgrade(out)));
                return Some(msg);
            }
        }
        None
    }

    /// 處理一行伺服器訊息。是我們請求的回應時回傳 [`Handled`]（呼叫端不再交給一般的處理）；
    /// 其他訊息回傳 `None`（回應 `heartbeat` 的 `ack` 會先取出 `pending`）
    pub fn on_line(&mut self, line: &str, last_sent_seq: u64) -> Option<Handled> {
        let msg = parse_server_msg(line).ok()?;
        let seq = match &msg {
            ServerMsg::Ack { seq, .. } => Some(*seq),
            ServerMsg::Error { seq, .. } => *seq,
            ServerMsg::Config(c) => Some(c.seq),
            ServerMsg::UpgradeOffer(o) => Some(o.seq),
            ServerMsg::UpgradeChunk(c) => Some(c.seq),
            _ => None,
        };
        let seq = seq.filter(|&s| s != 0 && s <= last_sent_seq);
        // 依序回應：比這個 seq 早的請求沒有收到回應
        if let Some(s) = seq {
            while let Some(&(q, _)) = self.outstanding.front() {
                if q >= s {
                    break;
                }
                if let Some((_, req)) = self.outstanding.pop_front() {
                    self.lost(req);
                }
            }
        }
        let mine = seq.is_some_and(|s| self.outstanding.front().is_some_and(|(q, _)| *q == s));
        if !mine {
            if let ServerMsg::Ack {
                pending: Some(p), ..
            } = &msg
            {
                self.on_pending(p);
            }
            // 不屬於遠端管理的回應（或伺服器主動送的、seq 對不上的遠端管理訊息）：交給一般的處理
            return match msg {
                ServerMsg::Config(_) | ServerMsg::UpgradeOffer(_) | ServerMsg::UpgradeChunk(_) => {
                    log::debug!("收到不屬於任何請求的遠端管理訊息（seq={seq:?}），忽略");
                    Some(Handled { answered: seq })
                }
                _ => None,
            };
        }
        let (s, req) = self.outstanding.pop_front()?;
        match (req, msg) {
            (Req::ConfigGet, ServerMsg::Config(c)) => {
                self.fetch_in_flight = false;
                let o = self.mgmt.config.apply(&c);
                if o.accepted_message {
                    self.send_status = true;
                    if o.reconnect {
                        self.reconnect_after_status = true;
                    }
                }
            }
            (Req::ConfigStatus(state), ServerMsg::Ack { .. }) => {
                self.mgmt.config.on_status_acked(&state);
                if self.reconnect_after_status && !self.send_status {
                    self.reconnect_now = true;
                }
            }
            (Req::Upgrade(Out::Get(_)), ServerMsg::UpgradeOffer(o)) => {
                self.mgmt.upgrade.on_offer(&mut self.upgrade, *o);
            }
            (
                Req::Upgrade(Out::Chunk {
                    filename,
                    offset,
                    length,
                    ..
                }),
                ServerMsg::UpgradeChunk(c),
            ) => {
                self.mgmt
                    .upgrade
                    .on_chunk(&mut self.upgrade, &c, (filename, offset, length));
            }
            (Req::Upgrade(Out::Status(r)), ServerMsg::Ack { .. }) => {
                self.mgmt.upgrade.on_status_acked(&mut self.upgrade, &r);
            }
            (
                req,
                ServerMsg::Error {
                    code,
                    message,
                    fatal: false,
                    retry_after_ms,
                    ..
                },
            ) => {
                log::debug!(
                    "遠端管理的請求（seq={s}）被拒：{}（{}）",
                    crate::network::sanitize_for_log(&code, 64),
                    crate::network::sanitize_for_log(&message, 200)
                );
                match req {
                    Req::ConfigGet => {
                        self.fetch_in_flight = false;
                    }
                    Req::ConfigStatus(_) => {
                        if self.reconnect_after_status {
                            self.reconnect_now = true;
                        }
                    }
                    Req::Upgrade(out) => {
                        self.mgmt
                            .upgrade
                            .on_error(&mut self.upgrade, &out, &code, retry_after_ms);
                    }
                }
            }
            (req, other) => {
                // 型別對不上（例如回應 config_get 的是 ack）：視為沒有回應
                log::debug!("遠端管理的請求（seq={s}）收到不相符的回應 {other:?}");
                self.lost(req);
                if matches!(other, ServerMsg::Error { fatal: true, .. }) {
                    // fatal error 交給一般的處理結束連線
                    return None;
                }
            }
        }
        Some(Handled { answered: Some(s) })
    }

    fn lost(&mut self, req: Req) {
        match req {
            Req::ConfigGet => self.fetch_in_flight = false,
            Req::ConfigStatus(_) => {
                if self.reconnect_after_status {
                    self.reconnect_now = true;
                }
            }
            Req::Upgrade(out) => self.mgmt.upgrade.on_lost(&mut self.upgrade, &out),
        }
    }

    fn on_pending(&mut self, p: &Pending) {
        if self.session.config {
            let (fetch, status, reconnect) = self.mgmt.config.on_pending(p);
            if fetch && !self.fetch_in_flight {
                self.fetch = true;
            }
            if status {
                self.send_status = true;
            }
            if reconnect {
                self.reconnect_after_status = true;
            }
        }
        if self.session.upgrade {
            self.mgmt.upgrade.on_pending(&mut self.upgrade, p);
        }
    }
}
