//! 單一連線的處理：TLS 握手 → 等待 hello → 已註冊狀態的訊息迴圈。

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, trace, warn};
use std::future::Future;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_util::codec::Framed;

use crate::auth::cert_matches_agent_id;
use crate::config::TimeoutSettings;
use crate::protocol::{
    ClientMsg, ErrorCode, Frame, MAX_CONSECUTIVE_BAD_REQUESTS, MAX_LINE_LENGTH, NdjsonCodec,
    PROTO_VERSION, ServerMsg, clamp_heartbeat_interval_ms, offline_after_ms, parse_client_msg,
    validate_client_msg, validate_hello, validate_report,
};
use crate::registry::{
    ClientChain, HelloInfo, KickReason, LastReport, RegisterKind, Registry, Touch,
};
use crate::shutdown::{CONNECTIONS_DEADLINE, Shutdown, drain_limit};
use crate::storage::{ReportRecord, ReportSink, fmt_time};
use crate::throttle::{Category, LogThrottle};
use crate::throttled;
use crate::tls::TlsState;

/// 送出 close_notify 後，讀取並丟棄對方剩餘資料的最長時間（平常；關機期間另有更短的上限）。
/// 若在對方仍在送資料時直接關閉 socket，作業系統會送出 RST，對方可能因此收不到最後的錯誤訊息。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// 握手失敗後，送出 alert 與 FIN 再讀取丟棄對方資料的上限。
/// 失敗多半來自掃描或設定錯誤，時間取短，避免被大量觸發時長時間佔用連線名額。
const HANDSHAKE_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// 讀取並丟棄 `stream` 的資料，直到對方關閉、發生錯誤或超過 `limit`；回傳丟棄的 bytes 數，逾時時為 `None`
async fn drain<S: AsyncRead + Unpin>(stream: &mut S, limit: Duration) -> Option<usize> {
    let mut buf = vec![0u8; 16 * 1024];
    timeout(limit, async {
        let mut total = 0usize;
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break total,
                Ok(n) => total += n,
            }
        }
    })
    .await
    .ok()
}

/// 所有連線共用的資源
pub struct ConnContext {
    pub registry: Arc<Registry>,
    pub reports: ReportSink,
    pub timeouts: TimeoutSettings,
    /// 伺服器關機狀態（取消訊號與 T0）
    pub shutdown: Arc<Shutdown>,
    /// 目前生效的 TLS 設定（熱重載時替換）；hello 時依它判斷是否要求憑證綁定
    pub tls: Arc<TlsState>,
    /// 可能被掃描大量觸發的日誌的節流器
    pub throttle: Arc<LogThrottle>,
}

type Transport = Framed<TlsStream<TcpStream>, NdjsonCodec>;

/// 處理完一則訊息後是否繼續
enum Flow {
    Continue,
    Close,
}

/// 已註冊狀態下處理一則訊息的結果
struct Handled {
    flow: Flow,
    /// 是否為合法訊息（hello、heartbeat、report，含 invalid_report）；只有合法訊息會重置閒置計時
    alive: bool,
}

impl Handled {
    fn alive(flow: Flow) -> Self {
        Self { flow, alive: true }
    }

    fn not_alive(flow: Flow) -> Self {
        Self { flow, alive: false }
    }
}

/// 讀到的一行經初步分類後的結果
enum Incoming {
    Msg(ClientMsg),
    TooLong,
    BadRequest(String),
}

fn classify(frame: Frame) -> Incoming {
    match frame {
        Frame::TooLong => Incoming::TooLong,
        Frame::InvalidUtf8 => Incoming::BadRequest("line is not valid UTF-8".to_string()),
        Frame::Line(line) => match parse_client_msg(&line) {
            Ok(msg) => match validate_client_msg(&msg) {
                Ok(()) => Incoming::Msg(msg),
                Err(e) => Incoming::BadRequest(e),
            },
            Err(e) => Incoming::BadRequest(e),
        },
    }
}

/// 計算 `now + duration` 的期限；溢位時退回較短的期限，避免 `Instant + Duration` panic
fn deadline_after(duration: Duration) -> Instant {
    const FALLBACK: Duration = Duration::from_secs(24 * 60 * 60);
    let now = Instant::now();
    now.checked_add(duration)
        .or_else(|| now.checked_add(FALLBACK))
        .unwrap_or(now)
}

/// 合法 report 的保存結果
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoreOutcome {
    /// 已寫入資料檔並更新 last_report；內含 last_seen 的更新結果
    Stored(Touch),
    /// 本連線已被取代，未寫檔也未更新任何狀態
    Stale,
    /// 寫檔失敗或逾時，資料未保存（不更新 last_report）
    Unavailable(String),
}

/// 保存一筆合法 report，順序保證 agents.json 與資料檔一致：
/// 1. 以 conn_id 確認仍是現役連線（並更新 last_seen），被取代的舊連線不寫檔；
/// 2. 交給 writer 並等待「已寫入並 flush」的回覆，整段受 `write_timeout` 限制；
/// 3. 寫入成功後才更新 last_report。失敗或逾時回 [`StoreOutcome::Unavailable`]。
pub(crate) async fn store_report(
    registry: &Registry,
    sink: &ReportSink,
    conn_id: u64,
    record: ReportRecord,
    write_timeout: Duration,
) -> StoreOutcome {
    let touch = registry.touch(
        &record.agent_id,
        conn_id,
        std::time::Instant::now(),
        Utc::now(),
    );
    if touch == Touch::Stale {
        return StoreOutcome::Stale;
    }
    let agent_id = record.agent_id.clone();
    let last = LastReport {
        recv_at: fmt_time(record.recv_at),
        seq: record.seq,
        ts: record.ts.clone(),
        cpu_usage: record.cpu_usage,
        total_memory: record.total_memory,
        used_memory: record.used_memory,
    };
    match timeout(write_timeout, sink.write(record)).await {
        Ok(Ok(())) => {
            // 寫入與此處之間若剛好被取代，資料已保存，只是不更新 last_report（新連線會有自己的 report）
            registry.set_last_report(&agent_id, conn_id, last);
            StoreOutcome::Stored(touch)
        }
        Ok(Err(reason)) => StoreOutcome::Unavailable(reason),
        Err(_) => {
            StoreOutcome::Unavailable(format!("寫入未在 {} 秒內完成", write_timeout.as_secs_f64()))
        }
    }
}

/// hello 通過後的 agent 資訊
struct Agent {
    agent_id: String,
    hostname: String,
    agent_version: String,
    heartbeat_interval_ms: u64,
    hello_seq: u64,
}

struct Conn {
    framed: Transport,
    conn_id: u64,
    peer: SocketAddr,
    write_timeout: Duration,
    shutdown: Arc<Shutdown>,
    tls: Arc<TlsState>,
    throttle: Arc<LogThrottle>,
    /// 握手時客戶端出示的憑證鏈（[0] 為 end-entity）；沒有出示時為 None
    client_chain: Option<ClientChain>,
    /// 連續 bad_request 次數
    bad_requests: u32,
}

/// 處理一條連線。握手失敗會在這裡（節流後）記錄並回傳 `Ok`；其他錯誤只影響此連線，由呼叫端記錄。
pub async fn handle_connection(
    ctx: Arc<ConnContext>,
    acceptor: TlsAcceptor,
    socket: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
) -> Result<(), Error> {
    let shutdown = ctx.shutdown.clone();
    // into_fallible：握手失敗時 tokio-rustls 會先送出 TLS alert，再把底層 socket 交還
    let handshake = tokio::select! {
        biased;
        // 握手尚未完成，無法送 close_notify，直接放棄
        _ = shutdown.cancelled() => return Ok(()),
        res = timeout(ctx.timeouts.handshake(), acceptor.accept(socket).into_fallible()) => res,
    };
    let tls_stream = match handshake {
        Ok(Ok(stream)) => stream,
        Ok(Err((e, mut tcp))) => {
            if is_peer_disconnect(&e) {
                debug!("[#{}] {} 在 TLS 握手期間中斷連線: {}", conn_id, peer, e);
            } else {
                // 無客戶端憑證、未知 CA、憑證已撤銷或過期、非 TLS 流量等；可能被掃描大量觸發，節流記錄
                throttled!(
                    ctx.throttle,
                    Category::Handshake,
                    warn,
                    "[#{}] {} TLS 握手失敗: {}",
                    conn_id,
                    peer,
                    e
                );
            }
            // alert 已送出；送 FIN 後讀取丟棄對方已送出的資料再關閉，避免 RST 讓對方收不到 alert
            let _ = tcp.shutdown().await;
            let limit = drain_limit(
                shutdown.started_at(),
                std::time::Instant::now(),
                HANDSHAKE_DRAIN_TIMEOUT,
            );
            drain(&mut tcp, limit).await;
            return Ok(());
        }
        Err(_) => {
            throttled!(
                ctx.throttle,
                Category::Handshake,
                warn,
                "[#{}] {} TLS 握手未在 {} 秒內完成，關閉連線",
                conn_id,
                peer,
                ctx.timeouts.handshake().as_secs()
            );
            return Ok(());
        }
    };
    debug!("[#{}] {} TLS 握手完成", conn_id, peer);
    let client_chain: Option<ClientChain> = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .filter(|certs| !certs.is_empty())
        .map(|certs| certs.iter().map(|c| c.clone().into_owned()).collect());

    let mut conn = Conn {
        framed: Framed::new(tls_stream, NdjsonCodec::new()),
        conn_id,
        peer,
        write_timeout: ctx.timeouts.write(),
        shutdown,
        tls: ctx.tls.clone(),
        throttle: ctx.throttle.clone(),
        client_chain,
        bad_requests: 0,
    };

    let Some(agent) = conn.await_hello(ctx.timeouts.hello()).await? else {
        return Ok(());
    };

    // 註冊後不論結果如何都要以 conn_id 比對並標記離線
    let result = conn.run_registered(&ctx, &agent).await;
    if ctx
        .registry
        .disconnect(&agent.agent_id, conn_id, std::time::Instant::now())
    {
        info!(
            "[#{}] agent {}（{}）連線關閉，標記為離線",
            conn_id,
            agent.agent_id,
            agent.hostname.escape_debug()
        );
    } else {
        debug!("[#{}] agent {} 的連線結束", conn_id, agent.agent_id);
    }
    result
}

impl Conn {
    async fn send(&mut self, msg: &ServerMsg) -> Result<(), Error> {
        let line = msg.to_line()?;
        with_timeout(self.write_timeout, "寫出訊息", self.framed.send(line)).await
    }

    /// 送出 close_notify 並關閉寫入方向，接著讀取並丟棄對方剩餘的資料，
    /// 直到對方關閉或超過時限，避免 RST 讓對方收不到最後的訊息。
    /// 平常最多讀 [`DRAIN_TIMEOUT`]；關機期間最多 200 ms，且送出與讀取都不超過連線收尾期限。
    async fn close(&mut self) -> Result<(), Error> {
        let close_timeout = match self.shutdown.started_at() {
            Some(t0) => self.write_timeout.min(
                (t0 + CONNECTIONS_DEADLINE).saturating_duration_since(std::time::Instant::now()),
            ),
            None => self.write_timeout,
        };
        with_timeout(close_timeout, "關閉連線", self.framed.close()).await?;
        let limit = drain_limit(
            self.shutdown.started_at(),
            std::time::Instant::now(),
            DRAIN_TIMEOUT,
        );
        match drain(self.framed.get_mut(), limit).await {
            Some(n) if n > 0 => trace!("[#{}] 關閉前丟棄 {} bytes", self.conn_id, n),
            Some(_) => {}
            None => debug!(
                "[#{}] {} 在 {} ms 內未關閉連線，直接釋放",
                self.conn_id,
                self.peer,
                limit.as_millis()
            ),
        }
        Ok(())
    }

    /// 送出回應；若為 fatal 錯誤則接著關閉連線
    async fn reply(&mut self, msg: ServerMsg) -> Result<Flow, Error> {
        self.send(&msg).await?;
        if msg.is_fatal() {
            self.close().await?;
            Ok(Flow::Close)
        } else {
            Ok(Flow::Continue)
        }
    }

    async fn reply_bad_request(&mut self, message: String) -> Result<Flow, Error> {
        self.bad_requests += 1;
        let fatal = self.bad_requests >= MAX_CONSECUTIVE_BAD_REQUESTS;
        if fatal {
            throttled!(
                self.throttle,
                Category::ProtocolViolation,
                warn,
                "[#{}] {} 連續 {} 則無效訊息，關閉連線（最後一則: {}）",
                self.conn_id,
                self.peer,
                self.bad_requests,
                message.escape_debug()
            );
        } else {
            debug!(
                "[#{}] {} 送出無效訊息: {}",
                self.conn_id,
                self.peer,
                message.escape_debug()
            );
        }
        self.reply(ServerMsg::error_with_fatal(
            None,
            ErrorCode::BadRequest,
            message,
            fatal,
        ))
        .await
    }

    async fn reply_too_long(&mut self) -> Result<Flow, Error> {
        throttled!(
            self.throttle,
            Category::ProtocolViolation,
            warn,
            "[#{}] {} 送出超過 {} bytes 的單行，關閉連線",
            self.conn_id,
            self.peer,
            MAX_LINE_LENGTH
        );
        self.reply(ServerMsg::error(
            None,
            ErrorCode::TooLong,
            format!("line exceeds {} bytes", MAX_LINE_LENGTH),
        ))
        .await
    }

    async fn reply_unknown_type(&mut self) -> Result<Flow, Error> {
        debug!("[#{}] {} 送出未知的訊息類型", self.conn_id, self.peer);
        self.reply(ServerMsg::error(
            None,
            ErrorCode::UnknownType,
            "unknown message type",
        ))
        .await
    }

    async fn reply_superseded(&mut self, agent: &Agent) -> Result<Flow, Error> {
        info!(
            "[#{}] agent {} 已由新連線取代，關閉舊連線",
            self.conn_id, agent.agent_id
        );
        self.reply(ServerMsg::error(
            None,
            ErrorCode::Superseded,
            "superseded by a newer connection",
        ))
        .await
    }

    /// 依目前的 TLS 設定（啟用 mTLS 時）檢查 hello 的 agent_id 是否與客戶端憑證相符；
    /// 不符則回 `auth_failed`（fatal）並關閉連線，回傳 `false`。
    /// 此檢查在 register 之前，因此冒用者無法踢掉既有連線。
    async fn check_identity(&mut self, seq: u64, agent_id: &str) -> Result<bool, Error> {
        if !self.tls.current().auth_enabled() {
            return Ok(true);
        }
        let result = match self.client_chain.as_deref().and_then(|c| c.first()) {
            Some(cert) => cert_matches_agent_id(cert, agent_id),
            None => Err("沒有客戶端憑證".to_string()),
        };
        match result {
            Ok(()) => Ok(true),
            Err(reason) => {
                warn!(
                    "[#{}] {} 的 hello agent_id \"{}\" 與客戶端憑證不符，關閉連線: {}",
                    self.conn_id,
                    self.peer,
                    agent_id.escape_debug(),
                    reason
                );
                self.reply(ServerMsg::error(
                    Some(seq),
                    ErrorCode::AuthFailed,
                    "agent_id does not match client certificate",
                ))
                .await?;
                Ok(false)
            }
        }
    }

    /// 伺服器關機：送出 close_notify 後結束
    async fn close_for_shutdown(&mut self) -> Result<(), Error> {
        debug!("[#{}] {} 伺服器關機，關閉連線", self.conn_id, self.peer);
        self.close().await
    }

    /// 握手後等待 hello。回傳 `None` 表示連線已結束（逾時、對方關閉、關機或 fatal 錯誤）。
    async fn await_hello(&mut self, hello_timeout: Duration) -> Result<Option<Agent>, Error> {
        // 以固定期限計算，無效訊息不會延長等待時間
        let deadline = deadline_after(hello_timeout);
        loop {
            let res = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => {
                    self.close_for_shutdown().await?;
                    return Ok(None);
                }
                res = timeout_at(deadline, self.framed.next()) => res,
            };
            let frame = match res {
                Err(_) => {
                    throttled!(
                        self.throttle,
                        Category::HelloTimeout,
                        info,
                        "[#{}] {} 在 {} 秒內未送出 hello，關閉連線",
                        self.conn_id,
                        self.peer,
                        hello_timeout.as_secs()
                    );
                    self.close().await?;
                    return Ok(None);
                }
                Ok(None) => {
                    debug!("[#{}] {} 在送出 hello 前關閉連線", self.conn_id, self.peer);
                    return Ok(None);
                }
                Ok(Some(Err(e))) => return Err(e),
                Ok(Some(Ok(frame))) => frame,
            };

            let flow = match classify(frame) {
                Incoming::TooLong => self.reply_too_long().await?,
                Incoming::BadRequest(message) => self.reply_bad_request(message).await?,
                Incoming::Msg(ClientMsg::Hello {
                    seq,
                    proto,
                    agent_id,
                    hostname,
                    agent_version,
                    heartbeat_interval_ms,
                    ..
                }) => {
                    if proto != PROTO_VERSION {
                        throttled!(
                            self.throttle,
                            Category::ProtocolViolation,
                            warn,
                            "[#{}] {} 的協定版本 {} 不受支援（agent_id={:?}），關閉連線",
                            self.conn_id,
                            self.peer,
                            proto,
                            agent_id
                        );
                        self.reply(ServerMsg::error(
                            Some(seq),
                            ErrorCode::UnsupportedVersion,
                            format!("supported proto: {}", PROTO_VERSION),
                        ))
                        .await?;
                        return Ok(None);
                    }
                    match validate_hello(&agent_id, &hostname, &agent_version) {
                        Ok(()) => {
                            if !self.check_identity(seq, &agent_id).await? {
                                return Ok(None);
                            }
                            self.bad_requests = 0;
                            return Ok(Some(Agent {
                                agent_id,
                                hostname,
                                agent_version,
                                heartbeat_interval_ms,
                                hello_seq: seq,
                            }));
                        }
                        // 欄位不合規定：計入連續 bad_request，繼續等待 hello
                        Err(message) => self.reply_bad_request(message).await?,
                    }
                }
                Incoming::Msg(ClientMsg::Heartbeat { seq, .. } | ClientMsg::Report { seq, .. }) => {
                    throttled!(
                        self.throttle,
                        Category::ProtocolViolation,
                        warn,
                        "[#{}] {} 未先送出 hello，關閉連線",
                        self.conn_id,
                        self.peer
                    );
                    self.reply(ServerMsg::error(
                        Some(seq),
                        ErrorCode::HelloRequired,
                        "hello required before other messages",
                    ))
                    .await?
                }
                Incoming::Msg(ClientMsg::Unknown) => {
                    self.bad_requests = 0;
                    self.reply_unknown_type().await?
                }
            };
            if let Flow::Close = flow {
                return Ok(None);
            }
        }
    }

    /// 註冊 agent 並處理後續訊息，直到連線結束
    async fn run_registered(&mut self, ctx: &ConnContext, agent: &Agent) -> Result<(), Error> {
        let clamped = clamp_heartbeat_interval_ms(agent.heartbeat_interval_ms);
        if clamped != agent.heartbeat_interval_ms {
            warn!(
                "[#{}] agent {} 宣告的 heartbeat_interval_ms={} 超出 1000..=60000，以 {} 計算離線門檻",
                self.conn_id, agent.agent_id, agent.heartbeat_interval_ms, clamped
            );
        }
        let offline_ms = offline_after_ms(agent.heartbeat_interval_ms);
        let offline_after = Duration::from_millis(offline_ms);

        let registration = ctx.registry.register(
            self.conn_id,
            HelloInfo {
                agent_id: agent.agent_id.clone(),
                hostname: agent.hostname.clone(),
                agent_version: agent.agent_version.clone(),
                peer: self.peer.to_string(),
                offline_after,
                client_chain: self.client_chain.clone(),
            },
            std::time::Instant::now(),
            Utc::now(),
        );
        let state = match registration.kind {
            RegisterKind::New => "上線".to_string(),
            RegisterKind::Reconnected => "重新上線".to_string(),
            RegisterKind::Superseded { previous_conn_id } => {
                format!("重新連線，取代舊連線 #{}", previous_conn_id)
            }
        };
        // hostname / agent_version 由對方提供：已在 hello 驗證拒絕控制字元，日誌輸出時再跳脫一次
        info!(
            "[#{}] agent {}（{}，v{}，{}）{}，離線門檻 {} ms",
            self.conn_id,
            agent.agent_id,
            agent.hostname.escape_debug(),
            agent.agent_version.escape_debug(),
            self.peer,
            state,
            offline_ms
        );

        self.send(&ServerMsg::HelloAck {
            seq: agent.hello_seq,
            proto: PROTO_VERSION,
            offline_after_ms: offline_ms,
        })
        .await?;

        let mut kick: oneshot::Receiver<KickReason> = registration.kick;
        let mut kick_armed = true;
        // 只有合法訊息會延後此期限；壞行與未知 type 不算
        let mut idle_deadline = deadline_after(offline_after);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => {
                    self.close_for_shutdown().await?;
                    return Ok(());
                }
                res = &mut kick, if kick_armed => match res {
                    Ok(KickReason::Superseded) => {
                        self.reply_superseded(agent).await?;
                        return Ok(());
                    }
                    Ok(KickReason::CertificateRejected(reason)) => {
                        info!(
                            "[#{}] agent {} 的客戶端憑證已被撤銷或不再受信任（{}），關閉連線",
                            self.conn_id, agent.agent_id, reason
                        );
                        self.reply(ServerMsg::error(
                            None,
                            ErrorCode::CertificateRejected,
                            "client certificate has been revoked or is no longer trusted",
                        ))
                        .await?;
                        return Ok(());
                    }
                    // sender 被釋放但沒有送出通知：之後不可能再被踢，停止監聽
                    Err(_) => kick_armed = false,
                },
                res = timeout_at(idle_deadline, self.framed.next()) => {
                    let frame = match res {
                        Err(_) => {
                            info!(
                                "[#{}] agent {} 超過 {} ms 沒有合法訊息，關閉連線",
                                self.conn_id, agent.agent_id, offline_ms
                            );
                            self.close().await?;
                            return Ok(());
                        }
                        Ok(None) => return Ok(()),
                        Ok(Some(Err(e))) => return Err(e),
                        Ok(Some(Ok(frame))) => frame,
                    };
                    let handled = self.handle_registered(ctx, agent, frame).await?;
                    if let Flow::Close = handled.flow {
                        return Ok(());
                    }
                    if handled.alive {
                        idle_deadline = deadline_after(offline_after);
                    }
                }
            }
        }
    }

    /// 處理 registry 更新結果。回傳 `false` 表示本連線已被取代。
    fn on_touch(&self, agent: &Agent, touch: Touch) -> bool {
        match touch {
            Touch::Updated => true,
            Touch::Revived => {
                info!(
                    "[#{}] agent {}（{}）恢復上線",
                    self.conn_id,
                    agent.agent_id,
                    agent.hostname.escape_debug()
                );
                true
            }
            Touch::Stale => false,
        }
    }

    /// 更新 last_seen。回傳 `false` 表示本連線已被取代。
    fn touch(&self, ctx: &ConnContext, agent: &Agent) -> bool {
        let touch = ctx.registry.touch(
            &agent.agent_id,
            self.conn_id,
            std::time::Instant::now(),
            Utc::now(),
        );
        self.on_touch(agent, touch)
    }

    async fn handle_registered(
        &mut self,
        ctx: &ConnContext,
        agent: &Agent,
        frame: Frame,
    ) -> Result<Handled, Error> {
        let msg = match classify(frame) {
            Incoming::TooLong => return Ok(Handled::not_alive(self.reply_too_long().await?)),
            Incoming::BadRequest(message) => {
                return Ok(Handled::not_alive(self.reply_bad_request(message).await?));
            }
            Incoming::Msg(msg) => msg,
        };
        self.bad_requests = 0;

        match msg {
            ClientMsg::Unknown => Ok(Handled::not_alive(self.reply_unknown_type().await?)),
            ClientMsg::Hello { seq, .. } => {
                if !self.touch(ctx, agent) {
                    return Ok(Handled::alive(self.reply_superseded(agent).await?));
                }
                debug!(
                    "[#{}] agent {} 重複送出 hello",
                    self.conn_id, agent.agent_id
                );
                let flow = self
                    .reply(ServerMsg::error(
                        Some(seq),
                        ErrorCode::AlreadyRegistered,
                        "hello already received on this connection",
                    ))
                    .await?;
                Ok(Handled::alive(flow))
            }
            ClientMsg::Heartbeat { seq, .. } => {
                if !self.touch(ctx, agent) {
                    return Ok(Handled::alive(self.reply_superseded(agent).await?));
                }
                trace!(
                    "[#{}] agent {} heartbeat seq={}",
                    self.conn_id, agent.agent_id, seq
                );
                Ok(Handled::alive(self.reply(ServerMsg::Ack { seq }).await?))
            }
            ClientMsg::Report {
                seq,
                ts,
                cpu_usage,
                total_memory,
                used_memory,
            } => {
                if let Err(message) = validate_report(cpu_usage, total_memory, used_memory) {
                    // 不寫檔，但仍視為存活
                    if !self.touch(ctx, agent) {
                        return Ok(Handled::alive(self.reply_superseded(agent).await?));
                    }
                    warn!(
                        "[#{}] agent {} 的 report seq={} 無效: {}",
                        self.conn_id, agent.agent_id, seq, message
                    );
                    let flow = self
                        .reply(ServerMsg::error(
                            Some(seq),
                            ErrorCode::InvalidReport,
                            message,
                        ))
                        .await?;
                    return Ok(Handled::alive(flow));
                }

                let record = ReportRecord {
                    recv_at: Utc::now(),
                    peer: self.peer.to_string(),
                    agent_id: agent.agent_id.clone(),
                    hostname: agent.hostname.clone(),
                    seq,
                    ts,
                    cpu_usage,
                    total_memory,
                    used_memory,
                };
                // ack 語意 = 已寫入資料檔；等待 writer 回覆（受寫出逾時限制）
                let outcome = store_report(
                    &ctx.registry,
                    &ctx.reports,
                    self.conn_id,
                    record,
                    self.write_timeout,
                )
                .await;
                match outcome {
                    StoreOutcome::Stale => {
                        debug!(
                            "[#{}] agent {} 的連線已被取代，report seq={} 不寫檔",
                            self.conn_id, agent.agent_id, seq
                        );
                        Ok(Handled::alive(self.reply_superseded(agent).await?))
                    }
                    StoreOutcome::Stored(touch) => {
                        self.on_touch(agent, touch);
                        debug!(
                            "[#{}] agent {} report seq={} cpu={:.2}% mem={}/{} MB",
                            self.conn_id, agent.agent_id, seq, cpu_usage, used_memory, total_memory
                        );
                        Ok(Handled::alive(self.reply(ServerMsg::Ack { seq }).await?))
                    }
                    StoreOutcome::Unavailable(reason) => {
                        // writer 已記錄 error；每個連線、每筆 report 都可能遇到，這裡只記 debug 避免洗版
                        debug!(
                            "[#{}] agent {} 的 report seq={} 未保存: {}",
                            self.conn_id, agent.agent_id, seq, reason
                        );
                        let flow = self
                            .reply(ServerMsg::error(
                                Some(seq),
                                ErrorCode::StorageUnavailable,
                                "report could not be stored",
                            ))
                            .await?;
                        Ok(Handled::alive(flow))
                    }
                }
            }
        }
    }
}

/// 對方未送 close_notify 就斷線（UnexpectedEof）、連線被重設（10054）或中止、寫入時管線已斷，
/// 這些都是 agent 端關機、斷網時的常見情況，不視為伺服器錯誤
pub fn is_peer_disconnect(e: &Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
    )
}

// 為非同步 I/O 加上逾時，逾時時轉換為 std::io::Error
pub async fn with_timeout<T>(
    duration: Duration,
    action: &str,
    fut: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    timeout(duration, fut)
        .await
        .map_err(|_| Error::new(ErrorKind::TimedOut, format!("{}逾時", action)))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{WriteRequest, report_channel};

    const AGENT: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";

    fn hello_info() -> HelloInfo {
        HelloInfo {
            agent_id: AGENT.into(),
            hostname: "web-01".into(),
            agent_version: "0.1.0".into(),
            peer: "127.0.0.1:1".into(),
            offline_after: Duration::from_secs(5),
            client_chain: None,
        }
    }

    fn record(seq: u64) -> ReportRecord {
        ReportRecord {
            recv_at: Utc::now(),
            peer: "p".into(),
            agent_id: AGENT.into(),
            hostname: "web-01".into(),
            seq,
            ts: "t".into(),
            cpu_usage: 1.0,
            total_memory: 2,
            used_memory: 1,
        }
    }

    fn last_report_seq(registry: &Registry) -> Option<u64> {
        registry.snapshot(Utc::now()).agents[0]
            .last_report
            .as_ref()
            .map(|r| r.seq)
    }

    /// 模擬 writer：取出一筆要求，以 `result` 回覆
    fn fake_writer(
        mut rx: tokio::sync::mpsc::Receiver<WriteRequest>,
        result: Result<(), String>,
    ) -> tokio::task::JoinHandle<Option<u64>> {
        tokio::spawn(async move {
            let req = rx.recv().await?;
            let seq = req.record.seq;
            let _ = req.reply.send(result);
            Some(seq)
        })
    }

    #[tokio::test]
    async fn store_report_success_updates_last_report() {
        let registry = Registry::new();
        let _reg = registry.register(1, hello_info(), std::time::Instant::now(), Utc::now());
        let (sink, rx) = report_channel(4);
        let writer = fake_writer(rx, Ok(()));

        let outcome = store_report(&registry, &sink, 1, record(7), Duration::from_secs(1)).await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Updated));
        assert_eq!(writer.await.unwrap(), Some(7));
        assert_eq!(last_report_seq(&registry), Some(7));
    }

    #[tokio::test]
    async fn store_report_write_failure_is_unavailable() {
        let registry = Registry::new();
        let _reg = registry.register(1, hello_info(), std::time::Instant::now(), Utc::now());
        let (sink, rx) = report_channel(4);
        let writer = fake_writer(rx, Err("disk full".into()));

        let outcome = store_report(&registry, &sink, 1, record(8), Duration::from_secs(1)).await;
        assert_eq!(outcome, StoreOutcome::Unavailable("disk full".into()));
        assert_eq!(writer.await.unwrap(), Some(8));
        // 未保存的資料不得出現在 last_report
        assert_eq!(last_report_seq(&registry), None);
    }

    #[tokio::test]
    async fn store_report_timeout_is_unavailable() {
        let registry = Registry::new();
        let _reg = registry.register(1, hello_info(), std::time::Instant::now(), Utc::now());
        // writer 取出要求後一直不回覆
        let (sink, mut rx) = report_channel(4);
        let holder = tokio::spawn(async move {
            let req = rx.recv().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(req);
        });

        let started = std::time::Instant::now();
        let outcome = store_report(&registry, &sink, 1, record(9), Duration::from_millis(50)).await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(last_report_seq(&registry), None);
        holder.abort();
    }

    #[tokio::test]
    async fn store_report_queue_full_times_out() {
        let registry = Registry::new();
        let _reg = registry.register(1, hello_info(), std::time::Instant::now(), Utc::now());
        // 容量 1 且已塞滿、沒人讀取 → 連放入佇列都會逾時
        let (sink, _rx) = report_channel(1);
        let s2 = sink.clone();
        let _stuck = tokio::spawn(async move { s2.write(record(1)).await });
        while sink.pending() < 1 {
            tokio::task::yield_now().await;
        }
        let outcome = store_report(&registry, &sink, 1, record(2), Duration::from_millis(50)).await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
        assert_eq!(last_report_seq(&registry), None);
    }

    #[tokio::test]
    async fn store_report_writer_stopped_is_unavailable() {
        let registry = Registry::new();
        let _reg = registry.register(1, hello_info(), std::time::Instant::now(), Utc::now());
        let (sink, rx) = report_channel(4);
        drop(rx);
        let outcome = store_report(&registry, &sink, 1, record(3), Duration::from_secs(1)).await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn store_report_from_superseded_connection_writes_nothing() {
        let registry = Registry::new();
        let _old = registry.register(1, hello_info(), std::time::Instant::now(), Utc::now());
        let _new = registry.register(2, hello_info(), std::time::Instant::now(), Utc::now());
        let (sink, mut rx) = report_channel(4);

        let outcome = store_report(&registry, &sink, 1, record(9), Duration::from_secs(1)).await;
        assert_eq!(outcome, StoreOutcome::Stale);
        assert!(rx.try_recv().is_err(), "被取代的連線不得寫檔");
        assert_eq!(last_report_seq(&registry), None);
    }

    #[test]
    fn peer_disconnect_kinds() {
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
        ] {
            assert!(is_peer_disconnect(&Error::new(kind, "x")));
        }
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::InvalidData,
            ErrorKind::Other,
        ] {
            assert!(!is_peer_disconnect(&Error::new(kind, "x")));
        }
    }

    #[test]
    fn deadline_after_does_not_panic_on_overflow() {
        let before = Instant::now();
        assert!(deadline_after(Duration::from_secs(5)) >= before + Duration::from_secs(5));
        // 極大值不得 panic
        assert!(deadline_after(Duration::MAX) >= before);
    }

    #[test]
    fn classify_rejects_long_ts() {
        let line = format!(
            r#"{{"type":"heartbeat","seq":1,"ts":"{}"}}"#,
            "x".repeat(65)
        );
        assert!(matches!(
            classify(Frame::Line(line)),
            Incoming::BadRequest(m) if m.contains("ts")
        ));
        let line = format!(
            r#"{{"type":"heartbeat","seq":1,"ts":"{}"}}"#,
            "x".repeat(64)
        );
        assert!(matches!(classify(Frame::Line(line)), Incoming::Msg(_)));
    }
}
