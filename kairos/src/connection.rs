//! 單一連線的處理：TLS 握手 → 等待 hello → 已註冊狀態的訊息迴圈。

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use log::{debug, trace};
use std::future::Future;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::UnixTime;
use tokio_rustls::server::TlsStream;
use tokio_util::codec::Framed;

use crate::auth::{BindingError, BindingPolicy, cert_matches_agent_id};
use crate::config::TimeoutSettings;
use crate::per_ip::source_key;
use crate::protocol::{
    ClientMsg, ErrorCode, Frame, MAX_CONSECUTIVE_BAD_REQUESTS, MAX_LINE_LENGTH, NdjsonCodec,
    PROTO_VERSION, Proto, ServerMsg, clamp_heartbeat_interval_ms, normalize_agent_id,
    offline_after_ms, parse_client_msg, validate_client_msg, validate_hello, validate_report,
};
use crate::rate::{
    MAX_REPORT_PACING_DELAY, Pace, REPORT_BURST, REPORT_RATE_PER_SEC, ReportLimiter, ReportSource,
    TokenBucket,
};
use crate::registry::{
    ClientChain, HelloInfo, KickReason, LastReport, RegisterKind, Registry, Touch,
};
use crate::shutdown::{CONNECTIONS_DEADLINE, Shutdown, drain_limit};
use crate::storage::{ReportRecord, ReportSink, WriteStatus, fmt_time};
use crate::throttle::{Category, LogThrottle};
use crate::throttled;
use crate::tls::{RevocationFailure, TlsState, revocation_failure, verify_chain};

/// 送出 close_notify 後，讀取並丟棄對方剩餘資料的最長時間（平常；關機期間另有更短的上限）。
/// 若在對方仍在送資料時直接關閉 socket，作業系統會送出 RST，對方可能因此收不到最後的錯誤訊息。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// 握手失敗後，送出 alert 與 FIN 再讀取丟棄對方資料的上限。
/// 失敗多半來自掃描或設定錯誤，時間取短，避免被大量觸發時長時間佔用連線名額。
const HANDSHAKE_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// 每條連線的訊息速率上限（所有訊息，含無效行；R2-K12）：可連續處理的則數
pub const MESSAGE_BURST: f64 = 50.0;

/// 每條連線每秒最多處理的訊息數。Pulse 正常每秒最多送出約 2 則，不會觸及。
/// 超過時延後處理（期間不讀取，由 TCP 對送出端施加背壓），每則最多延後 1 / 此值 秒。
pub const MESSAGES_PER_SEC: f64 = 20.0;

/// 單則訊息因訊息速率上限而延後的上限（= 1 / [`MESSAGES_PER_SEC`]）
pub const MAX_MESSAGE_PACING_DELAY: Duration = Duration::from_millis(50);

/// 等待寫檔逾時、但 writer 已開始寫入這筆（無法取消）時，額外等待的上限
const CLAIMED_WRITE_GRACE: Duration = Duration::from_secs(1);

/// 等待寫檔期間更新本連線 last_seen 的間隔。等待期間同一連線的 heartbeat 排在後面無法處理，
/// 最長可達 `write_secs` + 寬限 = 7 秒，超過最小的離線門檻 5 秒；定期更新避免 agent 被短暫標記離線。
/// 必須小於最小離線門檻（5 秒）。
const STORE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// 從收到 report 到送出回應（`ack` / `storage_unavailable`，或結果未知時斷線）的最壞延遲：
///
/// ```text
/// 訊息速率延後 MAX_MESSAGE_PACING_DELAY 0.05 s
/// + report 限速延後 MAX_REPORT_PACING_DELAY 0.5 s
/// + 等待寫檔 timeouts.write_secs     ≤ MAX_WRITE_SECS 6 s
/// + 寫入已開始時的寬限 CLAIMED_WRITE_GRACE 1 s
/// = 7.55 s ≤ 8 s
/// ```
///
/// 等待期間同一連線的 heartbeat 排在後面拿不到 `ack`。Pulse 的讀取閒置為
/// `max(2m, m + 10 s)`（m 為 Pulse 的心跳間隔）：Pulse 最後一次收到伺服器訊息後，
/// 最晚 m 秒內會送出下一則訊息，伺服器再於 8 秒內回應，`m + 8 s < m + 10 s`，
/// 因此對任何心跳間隔都不會被誤判為斷線。
pub const MAX_REPORT_RESPONSE_DELAY: Duration = Duration::from_secs(8);

// 編譯期檢查延遲預算（以毫秒計算）
const _: () = {
    let total_ms = MAX_MESSAGE_PACING_DELAY.as_millis()
        + MAX_REPORT_PACING_DELAY.as_millis()
        + (crate::config::MAX_WRITE_SECS as u128) * 1000
        + CLAIMED_WRITE_GRACE.as_millis();
    assert!(total_ms <= MAX_REPORT_RESPONSE_DELAY.as_millis());
    assert!(MAX_MESSAGE_PACING_DELAY.as_millis() * (MESSAGES_PER_SEC as u128) >= 1000);
};

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
    /// mTLS 名稱綁定的規則
    pub binding: BindingPolicy,
    /// 跨連線共用的 report 限速（R2-K3）
    pub report_limiter: Arc<ReportLimiter>,
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
    /// 已寫入資料檔並更新 last_report；內含等待期間各次 last_seen 更新的合併結果
    /// （任何一次為 [`Touch::Revived`] 即為 `Revived`）
    Stored(Touch),
    /// 本連線已被取代，未寫檔也未更新任何狀態
    Stale,
    /// 寫檔失敗或逾時，資料確定未保存（不更新 last_report）
    Unavailable(String),
    /// 逾時時 writer 已開始寫入這筆，無法確定是否已保存。不能回 `storage_unavailable`（未保存），
    /// 也不能回 `ack`，呼叫端應不回應直接關閉連線
    Unknown(String),
}

/// 保存一筆合法 report，順序保證 agents.json 與資料檔一致：
/// 1. 以 conn_id 確認仍是現役連線（並更新 last_seen），被取代的舊連線不寫檔；
/// 2. 交給 writer 並等待「已寫入並 flush」的回覆，整段受 `write_timeout` 限制；
///    逾時時先取消這筆要求，writer 保證不會再寫入，才回 [`StoreOutcome::Unavailable`]；
///    writer 已開始寫入而無法取消時，再等一小段，仍無結果則回 [`StoreOutcome::Unknown`]；
/// 3. 寫入成功後才更新 last_report，並再更新一次 last_seen（等待寫檔期間收不到 heartbeat）。
///
/// 各次更新 last_seen 中只要有一次讓 agent 從離線恢復，就把 `revived` 設為 `true`（不論保存結果），
/// 呼叫端據此記錄「恢復上線」（R2-K13）。
pub(crate) async fn store_report(
    registry: &Registry,
    sink: &ReportSink,
    conn_id: u64,
    record: ReportRecord,
    write_timeout: Duration,
    revived: &AtomicBool,
) -> StoreOutcome {
    let touch = registry.touch(
        &record.agent_id,
        conn_id,
        std::time::Instant::now(),
        Utc::now(),
    );
    match touch {
        Touch::Stale => return StoreOutcome::Stale,
        Touch::Revived => revived.store(true, Ordering::Relaxed),
        Touch::Updated => {}
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
    let timed_out = || format!("寫入未在 {} 秒內完成", write_timeout.as_secs_f64());
    let deadline = deadline_after(write_timeout);
    // 等待期間（含等待佇列空間）以 keep_alive_while 定期更新本連線的 last_seen
    // （只更新 conn_id 相符的現役連線）。在放入佇列前逾時：這筆根本沒有進入佇列
    let submit = keep_alive_while(registry, &agent_id, conn_id, revived, sink.submit(record));
    let mut pending = match timeout_at(deadline, submit).await {
        Ok(Ok(pending)) => pending,
        Ok(Err(reason)) => return StoreOutcome::Unavailable(reason),
        Err(_) => return StoreOutcome::Unavailable(timed_out()),
    };
    let status = match timeout_at(
        deadline,
        keep_alive_while(registry, &agent_id, conn_id, revived, pending.wait()),
    )
    .await
    {
        Ok(status) => status,
        // writer 尚未認領：取消成功後保證不會寫入
        Err(_) if pending.cancel() => return StoreOutcome::Unavailable(timed_out()),
        // writer 已在寫入這筆：結果很快就會出來，多等一小段
        Err(_) => match timeout(
            write_timeout.min(CLAIMED_WRITE_GRACE),
            keep_alive_while(registry, &agent_id, conn_id, revived, pending.wait()),
        )
        .await
        {
            Ok(status) => status,
            Err(_) => {
                return StoreOutcome::Unknown(format!("{}，且寫入已開始、結果未知", timed_out()));
            }
        },
    };
    match status {
        WriteStatus::Written => {
            // 寫入與此處之間若剛好被取代，資料已保存，只是不更新 last_report（新連線會有自己的 report）
            registry.set_last_report(&agent_id, conn_id, last);
            let touch = registry.touch(&agent_id, conn_id, std::time::Instant::now(), Utc::now());
            if touch == Touch::Revived {
                revived.store(true, Ordering::Relaxed);
            }
            StoreOutcome::Stored(if revived.load(Ordering::Relaxed) {
                Touch::Revived
            } else {
                touch
            })
        }
        WriteStatus::Failed(reason) => StoreOutcome::Unavailable(reason),
        WriteStatus::Unknown(reason) => StoreOutcome::Unknown(reason),
    }
}

/// 執行 `fut` 直到完成；期間每 [`STORE_KEEPALIVE_INTERVAL`] 更新一次本連線的 last_seen，
/// 讓 agent 從離線恢復時設定 `revived`
async fn keep_alive_while<F: Future>(
    registry: &Registry,
    agent_id: &str,
    conn_id: u64,
    revived: &AtomicBool,
    fut: F,
) -> F::Output {
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval_at(
        Instant::now() + STORE_KEEPALIVE_INTERVAL,
        STORE_KEEPALIVE_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            out = &mut fut => return out,
            _ = ticker.tick() => {
                // 已被取代（Stale）時不更新任何狀態；恢復上線（Revived）交給呼叫端記錄
                if registry.touch(agent_id, conn_id, std::time::Instant::now(), Utc::now())
                    == Touch::Revived
                {
                    revived.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

/// hello 通過後的 agent 資訊
struct Agent {
    /// 正規化（小寫）後的 agent_id
    agent_id: String,
    hostname: String,
    agent_version: String,
    heartbeat_interval_ms: u64,
    hello_seq: u64,
    /// 是否已以 mTLS 驗證 agent_id 與客戶端憑證綁定（決定 report 限速的來源）
    authenticated: bool,
}

/// 確保連線結束時（含 panic 與被中止）以 conn_id 標記離線（R2-K20）
struct DisconnectGuard {
    registry: Arc<Registry>,
    agent_id: String,
    conn_id: u64,
    armed: bool,
}

impl DisconnectGuard {
    fn new(registry: Arc<Registry>, agent_id: String, conn_id: u64) -> Self {
        Self {
            registry,
            agent_id,
            conn_id,
            armed: true,
        }
    }

    /// 正常結束時呼叫；回傳 `true` 表示這次呼叫把 agent 從線上改為離線
    fn disconnect(mut self) -> bool {
        self.armed = false;
        self.registry
            .disconnect(&self.agent_id, self.conn_id, std::time::Instant::now())
    }
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        if self.armed {
            // conn_id 不符（從未註冊或已被取代）時不做任何事
            self.registry
                .disconnect(&self.agent_id, self.conn_id, std::time::Instant::now());
        }
    }
}

struct Conn {
    framed: Transport,
    conn_id: u64,
    peer: SocketAddr,
    write_timeout: Duration,
    shutdown: Arc<Shutdown>,
    tls: Arc<TlsState>,
    throttle: Arc<LogThrottle>,
    binding: BindingPolicy,
    /// 握手時客戶端出示的憑證鏈（[0] 為 end-entity）；沒有出示時為 None
    client_chain: Option<ClientChain>,
    /// 連續無效訊息（bad_request 與 unknown_type）的次數；收到合法訊息時歸零
    bad_requests: u32,
    /// report 的速率限制：本連線的 bucket（未啟用 mTLS 時與來源 IP 的 bucket 一起使用）
    reports: TokenBucket,
    /// 跨連線共用的 report 限速
    report_limiter: Arc<ReportLimiter>,
    /// 所有訊息的速率限制
    messages: TokenBucket,
}

/// 處理一條連線。握手失敗會在這裡（節流後）記錄並回傳 `Ok`；其他錯誤只影響此連線，由呼叫端記錄。
/// `pre_auth` 為「握手中」連線的名額（R2-K2），在 hello 通過後（或連線結束時）釋放。
pub async fn handle_connection(
    ctx: Arc<ConnContext>,
    acceptor: TlsAcceptor,
    socket: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    pre_auth: Option<OwnedSemaphorePermit>,
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
            log_handshake_failure(&ctx.throttle, conn_id, peer, &e);
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
        binding: ctx.binding,
        client_chain,
        bad_requests: 0,
        reports: ReportLimiter::connection_bucket(std::time::Instant::now()),
        report_limiter: ctx.report_limiter.clone(),
        messages: TokenBucket::new(MESSAGE_BURST, MESSAGES_PER_SEC, std::time::Instant::now()),
    };

    let Some(agent) = conn.await_hello(ctx.timeouts.hello()).await? else {
        return Ok(());
    };
    // hello 已通過：不再佔用「握手中」的名額
    drop(pre_auth);

    // 註冊後不論結果如何（含 panic、被中止）都要以 conn_id 比對並標記離線
    let guard = DisconnectGuard::new(ctx.registry.clone(), agent.agent_id.clone(), conn_id);
    let result = conn.run_registered(&ctx, &agent).await;
    if guard.disconnect() {
        throttled!(
            ctx.throttle,
            (Category::AgentOffline, peer.ip()),
            info,
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

/// 握手失敗的日誌：撤銷與 CRL 狀態的錯誤與一般握手失敗分開節流，避免被掃描流量掩蓋
fn log_handshake_failure(throttle: &LogThrottle, conn_id: u64, peer: SocketAddr, e: &Error) {
    if is_peer_disconnect(e) {
        debug!("[#{}] {} 在 TLS 握手期間中斷連線: {}", conn_id, peer, e);
        return;
    }
    match revocation_failure(e) {
        Some(RevocationFailure::Revoked) => throttled!(
            throttle,
            (Category::CertificateRevoked, peer.ip()),
            warn,
            "[#{}] {} 出示的客戶端憑證已被撤銷，TLS 握手失敗: {}",
            conn_id,
            peer,
            e
        ),
        Some(RevocationFailure::StatusUnavailable) => throttled!(
            throttle,
            Category::RevocationStatus,
            error,
            "[#{}] {} TLS 握手失敗：無法判定客戶端憑證的撤銷狀態，CRL 可能已過期或缺少該層 CA 的 CRL，請更新 auth.crl: {}",
            conn_id,
            peer,
            e
        ),
        // 無客戶端憑證、未知 CA、過期、非 TLS 流量等；可能被掃描大量觸發，節流記錄
        None => throttled!(
            throttle,
            Category::Handshake,
            warn,
            "[#{}] {} TLS 握手失敗: {}",
            conn_id,
            peer,
            e
        ),
    }
}

impl Conn {
    async fn send(&mut self, msg: &ServerMsg) -> Result<(), Error> {
        let line = msg.to_line()?;
        with_timeout(self.write_timeout, "寫出訊息", self.framed.send(line)).await
    }

    /// 送出 close_notify 並關閉寫入方向，接著讀取並丟棄對方剩餘的資料，
    /// 直到對方關閉或超過時限，避免 RST 讓對方收不到最後的訊息。
    /// 平常最多讀 [`DRAIN_TIMEOUT`]；關機期間最多 200 ms，且送出與讀取都不超過連線收尾期限，
    /// 期限已過時直接放棄（連線會被釋放，對方視為斷線）。
    async fn close(&mut self) -> Result<(), Error> {
        let close_timeout = match self.shutdown.started_at() {
            Some(t0) => self.write_timeout.min(
                (t0 + CONNECTIONS_DEADLINE).saturating_duration_since(std::time::Instant::now()),
            ),
            None => self.write_timeout,
        };
        if close_timeout.is_zero() {
            debug!(
                "[#{}] {} 關機收尾期限已過，不送 close_notify 直接釋放連線",
                self.conn_id, self.peer
            );
            return Ok(());
        }
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

    /// 回覆無效訊息（`bad_request` 或 `unknown_type`）。兩者合計連續達上限（中間沒有合法訊息）時，
    /// 該則改為 fatal 並關閉連線，避免交錯送出兩種無效訊息來繞過上限。
    async fn reply_invalid(&mut self, code: ErrorCode, message: String) -> Result<Flow, Error> {
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
                "[#{}] {} 送出無效訊息（{}）: {}",
                self.conn_id,
                self.peer,
                code.as_str(),
                message.escape_debug()
            );
        }
        self.reply(ServerMsg::error_with_fatal(None, code, message, fatal))
            .await
    }

    async fn reply_bad_request(&mut self, message: String) -> Result<Flow, Error> {
        self.reply_invalid(ErrorCode::BadRequest, message).await
    }

    async fn reply_unknown_type(&mut self) -> Result<Flow, Error> {
        self.reply_invalid(ErrorCode::UnknownType, "unknown message type".to_string())
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

    async fn reply_superseded(&mut self, agent: &Agent) -> Result<Flow, Error> {
        throttled!(
            self.throttle,
            (Category::AgentOffline, self.peer.ip()),
            info,
            "[#{}] agent {} 已由新連線取代，關閉舊連線",
            self.conn_id,
            agent.agent_id
        );
        self.reply(ServerMsg::error(
            None,
            ErrorCode::Superseded,
            "superseded by a newer connection",
        ))
        .await
    }

    /// 啟用 mTLS 時（依目前的 TLS 設定）在 register 之前檢查客戶端憑證，因此冒用者無法踢掉既有連線：
    /// 1. 以目前的驗證器重新驗證憑證鏈（握手後才撤銷、過期，或握手後重載了 CA/CRL）；
    ///    不通過回 `certificate_rejected`；
    /// 2. hello 的 agent_id 必須與憑證的 SAN 相符（預設憑證只能有一個 DNS SAN）；不符回 `auth_failed`。
    ///
    /// 兩者都是 fatal，關閉連線並回傳 `None`；通過時回傳是否已以 mTLS 驗證。
    async fn check_identity(&mut self, seq: u64, agent_id: &str) -> Result<Option<bool>, Error> {
        let material = self.tls.current();
        let Some(verifier) = &material.verifier else {
            return Ok(Some(false));
        };
        if let Err(reason) = verify_chain(
            verifier.as_ref(),
            self.client_chain.as_deref(),
            UnixTime::now(),
        ) {
            throttled!(
                self.throttle,
                (Category::AuthFailed, self.peer.ip()),
                warn,
                "[#{}] {} 的客戶端憑證在 hello 時已不再有效（agent_id \"{}\"），關閉連線: {}",
                self.conn_id,
                self.peer,
                agent_id.escape_debug(),
                reason
            );
            // CRL 過期或無法判定撤銷狀態時，message 為 `revocation status unavailable`（R2-K26）
            self.reply(ServerMsg::error(
                Some(seq),
                ErrorCode::CertificateRejected,
                reason.client_message(),
            ))
            .await?;
            return Ok(None);
        }
        let result = match self.client_chain.as_deref().and_then(|c| c.first()) {
            Some(cert) => cert_matches_agent_id(cert, agent_id, self.binding),
            None => Err(BindingError::Mismatch("沒有客戶端憑證".to_string())),
        };
        match result {
            Ok(()) => Ok(Some(true)),
            Err(reason) => {
                throttled!(
                    self.throttle,
                    (Category::AuthFailed, self.peer.ip()),
                    warn,
                    "[#{}] {} 的 hello agent_id \"{}\" 未通過客戶端憑證綁定，關閉連線: {}",
                    self.conn_id,
                    self.peer,
                    agent_id.escape_debug(),
                    reason
                );
                self.reply(ServerMsg::error(
                    Some(seq),
                    ErrorCode::AuthFailed,
                    reason.client_message(),
                ))
                .await?;
                Ok(None)
            }
        }
    }

    /// 訊息速率上限：超過時延後處理（期間不讀取，TCP 背壓會讓送出端慢下來；R2-K12）
    async fn pace_message(&mut self) {
        let wait = self
            .messages
            .acquire(std::time::Instant::now())
            .min(MAX_MESSAGE_PACING_DELAY);
        if wait.is_zero() {
            return;
        }
        throttled!(
            self.throttle,
            (Category::MessageRate, self.peer.ip()),
            warn,
            "[#{}] {} 送出訊息超過每秒 {} 則（可連續 {} 則），延後處理",
            self.conn_id,
            self.peer,
            MESSAGES_PER_SEC,
            MESSAGE_BURST
        );
        tokio::time::sleep(wait).await;
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
            self.pace_message().await;

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
                    if proto != Proto::Version(PROTO_VERSION) {
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
                            // UUID 不分大小寫：之後的驗證、註冊、日誌與寫檔一律使用小寫
                            let agent_id = normalize_agent_id(&agent_id);
                            let Some(authenticated) = self.check_identity(seq, &agent_id).await?
                            else {
                                return Ok(None);
                            };
                            self.bad_requests = 0;
                            return Ok(Some(Agent {
                                agent_id,
                                hostname,
                                agent_version,
                                heartbeat_interval_ms,
                                hello_seq: seq,
                                authenticated,
                            }));
                        }
                        // 欄位不合規定：計入連續無效訊息，繼續等待 hello
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
                // 不重置連續無效訊息的計數（避免與 bad_request 交錯送出來繞過上限）
                Incoming::Msg(ClientMsg::Unknown) => self.reply_unknown_type().await?,
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
            throttled!(
                self.throttle,
                (Category::HeartbeatClamped, self.peer.ip()),
                warn,
                "[#{}] agent {} 宣告的 heartbeat_interval_ms={} 超出 1000..=60000，以 {} 計算離線門檻",
                self.conn_id,
                agent.agent_id,
                agent.heartbeat_interval_ms,
                clamped
            );
        }
        let offline_ms = offline_after_ms(agent.heartbeat_interval_ms);
        let offline_after = Duration::from_millis(offline_ms);

        let registered = ctx.registry.register(
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
        let registration = match registered {
            Ok(registration) => registration,
            Err(full) => {
                throttled!(
                    self.throttle,
                    Category::RegistryFull,
                    warn,
                    "[#{}] 狀態表已達上限 {}（limits.max_agents）且沒有可淘汰的離線 agent，拒絕 {} 的新 agent {}",
                    self.conn_id,
                    full.max_agents,
                    self.peer,
                    agent.agent_id
                );
                // 防禦性：設定保證走不到（見 RegistryFull）。沒有專用的錯誤碼；
                // 以 fatal 的 bad_request 拒絕（Pulse 在 hello_ack 前收到錯誤會退避重連）
                self.reply(ServerMsg::error_with_fatal(
                    Some(agent.hello_seq),
                    ErrorCode::BadRequest,
                    "server agent registry is full",
                    true,
                ))
                .await?;
                return Ok(());
            }
        };
        if let Some(evicted) = &registration.evicted {
            throttled!(
                self.throttle,
                Category::RegistryEvicted,
                info,
                "狀態表已達上限（limits.max_agents），移除離線最久的 agent {}（{}，已離線 {:.1} 小時）",
                evicted.agent_id,
                evicted.hostname.escape_debug(),
                evicted.offline_for.as_secs_f64() / 3600.0
            );
        }
        let state = match registration.kind {
            RegisterKind::New => "上線".to_string(),
            RegisterKind::Reconnected => "重新上線".to_string(),
            RegisterKind::Superseded { previous_conn_id } => {
                format!("重新連線，取代舊連線 #{}", previous_conn_id)
            }
        };
        // hostname / agent_version 由對方提供：已在 hello 驗證拒絕控制與格式字元，日誌輸出時再跳脫一次
        throttled!(
            self.throttle,
            (Category::AgentOnline, self.peer.ip()),
            info,
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
                        // 每條連線最多一次，數量受連線數限制；reload 也會記錄一行 warn
                        debug!(
                            "[#{}] agent {} 的客戶端憑證已被撤銷、不再受信任或無法判定撤銷狀態（{}），關閉連線",
                            self.conn_id, agent.agent_id, reason
                        );
                        self.reply(ServerMsg::error(
                            None,
                            ErrorCode::CertificateRejected,
                            reason.client_message(),
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
                            throttled!(
                                self.throttle,
                                (Category::AgentOffline, self.peer.ip()),
                                info,
                                "[#{}] agent {} 超過 {} ms 沒有合法訊息，關閉連線",
                                self.conn_id,
                                agent.agent_id,
                                offline_ms
                            );
                            self.close().await?;
                            return Ok(());
                        }
                        Ok(None) => return Ok(()),
                        Ok(Some(Err(e))) => return Err(e),
                        Ok(Some(Ok(frame))) => frame,
                    };
                    // 讀到這一行的時間：之後的限速等待不影響 report 的 recv_at（R2-K16）
                    let received_at = Utc::now();
                    self.pace_message().await;
                    let handled = self.handle_registered(ctx, agent, frame, received_at).await?;
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
                throttled!(
                    self.throttle,
                    (Category::AgentOnline, self.peer.ip()),
                    info,
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

    /// report 速率限制（跨連線；R2-K3）：超過時延後處理（期間不讀取，TCP 背壓會讓送出端慢下來）。
    /// 需要延後超過 [`MAX_REPORT_PACING_DELAY`]（同一來源有多條連線同時送出）時回傳 `false`，
    /// 這筆不處理，回 `storage_unavailable`（確定未保存）。
    async fn pace_report(&mut self, agent: &Agent) -> bool {
        let now = std::time::Instant::now();
        let pace = if agent.authenticated {
            // mTLS：agent_id 與憑證綁定，以 agent_id 為來源
            self.report_limiter.acquire(
                ReportSource::Agent(agent.agent_id.clone()),
                None,
                now,
                MAX_REPORT_PACING_DELAY,
            )
        } else {
            // agent_id 可任意宣告：以來源 IP（IPv6 /64）為來源，並限制本連線
            self.report_limiter.acquire(
                ReportSource::Ip(source_key(self.peer.ip())),
                Some(&mut self.reports),
                now,
                MAX_REPORT_PACING_DELAY,
            )
        };
        let wait = match pace {
            Pace::Proceed(wait) if wait.is_zero() => return true,
            Pace::Proceed(wait) => Some(wait.min(MAX_REPORT_PACING_DELAY)),
            Pace::Reject => None,
        };
        throttled!(
            self.throttle,
            (Category::ReportRate, self.peer.ip()),
            warn,
            "[#{}] agent {} 的 report 超過每秒 {} 則（可連續 {} 則），{}",
            self.conn_id,
            agent.agent_id,
            REPORT_RATE_PER_SEC,
            REPORT_BURST,
            if wait.is_some() {
                "延後處理"
            } else {
                "不保存並回 storage_unavailable"
            }
        );
        match wait {
            Some(wait) => {
                tokio::time::sleep(wait).await;
                true
            }
            None => false,
        }
    }

    async fn handle_registered(
        &mut self,
        ctx: &ConnContext,
        agent: &Agent,
        frame: Frame,
        received_at: DateTime<Utc>,
    ) -> Result<Handled, Error> {
        let msg = match classify(frame) {
            Incoming::TooLong => return Ok(Handled::not_alive(self.reply_too_long().await?)),
            Incoming::BadRequest(message) => {
                return Ok(Handled::not_alive(self.reply_bad_request(message).await?));
            }
            Incoming::Msg(msg) => msg,
        };

        // 只有合法訊息（hello、heartbeat、report）會把連續無效訊息的計數歸零
        match msg {
            // 未知 type 不是合法訊息：不重置計數，也不延後閒置期限
            ClientMsg::Unknown => Ok(Handled::not_alive(self.reply_unknown_type().await?)),
            ClientMsg::Hello { seq, .. } => {
                self.bad_requests = 0;
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
                self.bad_requests = 0;
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
                self.bad_requests = 0;
                if !self.pace_report(agent).await {
                    if !self.touch(ctx, agent) {
                        return Ok(Handled::alive(self.reply_superseded(agent).await?));
                    }
                    let flow = self
                        .reply(ServerMsg::error(
                            Some(seq),
                            ErrorCode::StorageUnavailable,
                            "report rate limit exceeded",
                        ))
                        .await?;
                    return Ok(Handled::alive(flow));
                }
                if let Err(message) = validate_report(cpu_usage, total_memory, used_memory) {
                    // 不寫檔，但仍視為存活
                    if !self.touch(ctx, agent) {
                        return Ok(Handled::alive(self.reply_superseded(agent).await?));
                    }
                    throttled!(
                        self.throttle,
                        (Category::InvalidReport, self.peer.ip()),
                        warn,
                        "[#{}] agent {} 的 report seq={} 無效: {}",
                        self.conn_id,
                        agent.agent_id,
                        seq,
                        message
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
                    recv_at: received_at,
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
                let revived = AtomicBool::new(false);
                let outcome = store_report(
                    &ctx.registry,
                    &ctx.reports,
                    self.conn_id,
                    record,
                    self.write_timeout,
                    &revived,
                )
                .await;
                // 等待期間任何一次更新讓 agent 從離線恢復，都記錄「恢復上線」（R2-K13）
                if revived.load(Ordering::Relaxed) {
                    self.on_touch(agent, Touch::Revived);
                }
                match outcome {
                    StoreOutcome::Stale => {
                        debug!(
                            "[#{}] agent {} 的連線已被取代，report seq={} 不寫檔",
                            self.conn_id, agent.agent_id, seq
                        );
                        Ok(Handled::alive(self.reply_superseded(agent).await?))
                    }
                    StoreOutcome::Stored(_) => {
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
                    StoreOutcome::Unknown(reason) => {
                        // 既不能說「已保存」也不能說「未保存」：不回應，直接關閉連線
                        throttled!(
                            self.throttle,
                            Category::WriteUnknown,
                            warn,
                            "[#{}] agent {} 的 report seq={} 寫入結果未知（{}），不回應並關閉連線",
                            self.conn_id,
                            agent.agent_id,
                            seq,
                            reason
                        );
                        self.close().await?;
                        Ok(Handled::alive(Flow::Close))
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

    /// 模擬 writer：取出一筆要求、認領後以 `result` 回覆
    fn fake_writer(
        mut rx: tokio::sync::mpsc::Receiver<WriteRequest>,
        result: Result<(), String>,
    ) -> tokio::task::JoinHandle<Option<u64>> {
        tokio::spawn(async move {
            let req = rx.recv().await?;
            assert!(req.ticket.claim());
            let seq = req.record.seq;
            let status = match result {
                Ok(()) => WriteStatus::Written,
                Err(reason) => WriteStatus::Failed(reason),
            };
            let _ = req.reply.send(status);
            Some(seq)
        })
    }

    #[tokio::test]
    async fn store_report_success_updates_last_report() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, rx) = report_channel(4);
        let writer = fake_writer(rx, Ok(()));

        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(7),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Updated));
        assert_eq!(writer.await.unwrap(), Some(7));
        assert_eq!(last_report_seq(&registry), Some(7));
    }

    #[tokio::test]
    async fn store_report_write_failure_is_unavailable() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, rx) = report_channel(4);
        let writer = fake_writer(rx, Err("disk full".into()));

        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(8),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Unavailable("disk full".into()));
        assert_eq!(writer.await.unwrap(), Some(8));
        // 未保存的資料不得出現在 last_report
        assert_eq!(last_report_seq(&registry), None);
    }

    #[tokio::test]
    async fn store_report_timeout_is_unavailable_and_cancels_the_write() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        // writer 一直不處理（尚未認領）
        let (sink, mut rx) = report_channel(4);
        let started = std::time::Instant::now();
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(9),
            Duration::from_millis(50),
            &AtomicBool::new(false),
        )
        .await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(last_report_seq(&registry), None);
        // K-C1：已回 storage_unavailable 的要求，writer 之後不得寫入（認領失敗）
        let req = rx.recv().await.unwrap();
        assert!(!req.ticket.claim());
    }

    #[tokio::test]
    async fn store_report_claimed_but_unfinished_is_unknown() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        // writer 認領後卡住（例如磁碟沒有回應）
        let (sink, mut rx) = report_channel(4);
        let holder = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(req);
        });
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(9),
            Duration::from_millis(50),
            &AtomicBool::new(false),
        )
        .await;
        // 不能回 storage_unavailable（資料可能已寫入），也不能回 ack
        assert!(matches!(outcome, StoreOutcome::Unknown(_)), "{outcome:?}");
        assert_eq!(last_report_seq(&registry), None);
        holder.abort();
    }

    // R2-T2：以暫停的 tokio 時鐘執行，邊界判定不受實際排程延遲影響
    #[tokio::test(start_paused = true)]
    async fn store_report_claimed_write_finishing_within_grace_is_stored() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            // 超過逾時（500 ms）才完成，但在寬限（再 500 ms）內
            tokio::time::sleep(Duration::from_millis(800)).await;
            let _ = req.reply.send(WriteStatus::Written);
        });
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(11),
            Duration::from_millis(500),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Updated));
        assert_eq!(last_report_seq(&registry), Some(11));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn store_report_touches_again_after_slow_write() {
        // K-C4：等待寫檔期間被 sweep 判定離線，寫入完成後應恢復上線
        let registry = Arc::new(Registry::new());
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let r = registry.clone();
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            let swept = r.sweep(std::time::Instant::now() + Duration::from_secs(60));
            assert_eq!(swept.went_offline.len(), 1);
            let _ = req.reply.send(WriteStatus::Written);
        });
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(12),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Revived));
        assert_eq!(registry.counts(), (1, 1));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn slow_disk_longer_than_offline_threshold_does_not_mark_agent_offline() {
        // K-C4：磁碟卡住超過離線門檻（此處 3 秒）時，等待期間每秒 touch 本連線，不會被 sweep 標記離線。
        // R2-T2：touch 間隔 1 秒與門檻 3 秒之間留 2 秒餘裕，卡住時間比門檻多 1.5 秒
        // （registry 以 std 的時鐘判定離線，無法使用暫停的 tokio 時鐘）
        let registry = Arc::new(Registry::new());
        let mut info = hello_info();
        info.offline_after = Duration::from_secs(3);
        let _reg = registry
            .register(1, info, std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            tokio::time::sleep(Duration::from_millis(4500)).await;
            let _ = req.reply.send(WriteStatus::Written);
        });
        // 模擬 monitor：每 100 ms sweep 一次
        let r = registry.clone();
        let sweeper = tokio::spawn(async move {
            let mut went_offline = 0;
            loop {
                went_offline += r.sweep(std::time::Instant::now()).went_offline.len();
                if went_offline > 0 {
                    return went_offline;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(13),
            Duration::from_secs(6),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Updated));
        assert!(!sweeper.is_finished(), "等待寫檔期間不應被標記離線");
        sweeper.abort();
        assert_eq!(registry.counts(), (1, 1));
        writer.await.unwrap();

        // 已被取代的舊連線等待期間不會 touch 到新連線的狀態
        let _new = registry
            .register(2, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        assert_eq!(
            registry.touch(AGENT, 1, std::time::Instant::now(), Utc::now()),
            Touch::Stale
        );
    }

    #[tokio::test]
    async fn store_report_queue_full_times_out() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        // 容量 1 且已塞滿、沒人讀取 → 連放入佇列都會逾時
        let (sink, _rx) = report_channel(1);
        let s2 = sink.clone();
        let _stuck = tokio::spawn(async move { s2.write(record(1)).await });
        while sink.pending() < 1 {
            tokio::task::yield_now().await;
        }
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(2),
            Duration::from_millis(50),
            &AtomicBool::new(false),
        )
        .await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
        assert_eq!(last_report_seq(&registry), None);
        // 逾時的那筆沒有進入佇列
        assert_eq!(sink.pending(), 1);
    }

    #[tokio::test]
    async fn store_report_writer_stopped_is_unavailable() {
        let registry = Registry::new();
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, rx) = report_channel(4);
        drop(rx);
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(3),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert!(
            matches!(outcome, StoreOutcome::Unavailable(_)),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn store_report_from_superseded_connection_writes_nothing() {
        let registry = Registry::new();
        let _old = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let _new = registry
            .register(2, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);

        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(9),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stale);
        assert!(rx.try_recv().is_err(), "被取代的連線不得寫檔");
        assert_eq!(last_report_seq(&registry), None);
    }

    #[tokio::test(start_paused = true)]
    async fn revival_during_keepalive_is_reported() {
        // R2-K13／R2-T2：等待寫檔期間被 sweep 判定離線，由「等待中的定期 touch」恢復上線
        // （寫入完成後的 touch 只看到 Updated），仍要回報恢復上線
        let registry = Arc::new(Registry::new());
        let _reg = registry
            .register(1, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let (sink, mut rx) = report_channel(4);
        let r = registry.clone();
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            assert_eq!(
                r.sweep(std::time::Instant::now() + Duration::from_secs(60))
                    .went_offline
                    .len(),
                1
            );
            // 跨過至少一次定期 touch（每 1 秒）
            tokio::time::sleep(STORE_KEEPALIVE_INTERVAL * 2).await;
            assert_eq!(r.counts(), (1, 1), "定期 touch 已恢復上線");
            let _ = req.reply.send(WriteStatus::Written);
        });
        let revived = AtomicBool::new(false);
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(14),
            Duration::from_secs(5),
            &revived,
        )
        .await;
        assert_eq!(outcome, StoreOutcome::Stored(Touch::Revived));
        assert!(revived.load(Ordering::Relaxed));
        writer.await.unwrap();

        // 寫入失敗時也回報（恢復上線與是否保存無關）
        let (sink, mut rx) = report_channel(4);
        let r = registry.clone();
        let writer = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(req.ticket.claim());
            r.sweep(std::time::Instant::now() + Duration::from_secs(60));
            tokio::time::sleep(STORE_KEEPALIVE_INTERVAL * 2).await;
            let _ = req.reply.send(WriteStatus::Failed("disk full".into()));
        });
        let revived = AtomicBool::new(false);
        let outcome = store_report(
            &registry,
            &sink,
            1,
            record(15),
            Duration::from_secs(5),
            &revived,
        )
        .await;
        assert!(matches!(outcome, StoreOutcome::Unavailable(_)));
        assert!(revived.load(Ordering::Relaxed));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_guard_marks_offline_even_if_the_task_panics() {
        // R2-K20：連線工作 panic 時仍以 conn_id 標記離線
        let registry = Arc::new(Registry::new());
        let r = registry.clone();
        let task = tokio::spawn(async move {
            let _reg = r
                .register(7, hello_info(), std::time::Instant::now(), Utc::now())
                .unwrap();
            let _guard = DisconnectGuard::new(r.clone(), AGENT.to_string(), 7);
            panic!("連線工作異常");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert_eq!(registry.counts(), (0, 1));

        // 正常結束：disconnect 回報狀態變化，之後 drop 不重複處理
        let _reg = registry
            .register(8, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        let guard = DisconnectGuard::new(registry.clone(), AGENT.to_string(), 8);
        assert!(guard.disconnect());
        // 已被新連線取代的舊連線：不影響新連線
        let _new = registry
            .register(9, hello_info(), std::time::Instant::now(), Utc::now())
            .unwrap();
        drop(DisconnectGuard::new(registry.clone(), AGENT.to_string(), 8));
        assert_eq!(registry.counts(), (1, 1));
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
        // R2-K8：ts 含行分隔字元 → bad_request
        let line = r#"{"type":"heartbeat","seq":1,"ts":"2026 x"}"#.to_string();
        assert!(matches!(
            classify(Frame::Line(line)),
            Incoming::BadRequest(m) if m.contains("ts")
        ));
    }
}
