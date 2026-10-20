//! 8444 的端點（`crates/kairos-proto/ENROLL.md` 第 4～8 節）。
//!
//! 每個端點依規格的判定順序逐層檢查；被前一步拒絕的請求不扣後面各層的額度。
//!
//! | 端點 | 判定順序 |
//! |---|---|
//! | `server-info` | L4 |
//! | `token-check` | L7 封鎖 → L4 → 模式 → 本文 → token |
//! | 新註冊 | 第 6.3 節 1～15 |
//! | 輪詢 | `wait` → 輪詢憑據（不符計入 L7）→ L10 → 長輪詢 |
//! | 過期續簽 | 第 7.2 節 1～16 |
//! | 撤回 | L10 → nonce → 簽章（不符計入 L7 續簽類）→ 狀態 |

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use kairos_proto::SUPPORTED_PROTO;
use kairos_proto::enroll::{
    Accepts, CheckedEnrollRequest, ENROLL_API_VERSION, ENROLL_FEATURE_GRACE_RENEW,
    ENROLL_FEATURE_LONG_POLL, ENROLL_FEATURE_WITHDRAW, EndpointClass, EnrollErrorCode,
    EnrollRequest, EnrollResult, GraceRenewRequest, IssuedCertificate, ModeDecision, RateScope,
    RequestCode, Route, RouteError, ServerInfo, ServerVerification, TokenApproval,
    TokenCheckRequest, TokenCheckResponse, TokenStatus, WithdrawChallenge, WithdrawRequest,
    WithdrawResult, b64url_encode, clock_offset_ms, exceeds_clock_skew, format_rfc3339_ms,
    grace_renew_signing_input, mode_decision, parse_poll_authorization, parse_route,
    parse_wait_secs, replay_cache_key, request_code_from_spki, sha256, validate_enroll_request,
    validate_grace_renew_request, validate_token_check, validate_withdraw_request,
    verify_ecdsa_p256_sha256, withdraw_signing_input,
};
use log::{debug, info, warn};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

use super::db::DbError;
use super::http::{self as h, EnrollError};
use super::limits::InvalidClass;
use super::pki_mgr::{IssueError, IssueOutcome};
use super::store::{Audit, DbKind, DbState, RequestRow, TokenRow, TokenState};
use super::{EnrollService, ModeState};
use crate::api::http::ApiBody;
use crate::per_ip::source_key;
use crate::pki::csr::check_csr;
use crate::pki::store::CertRow;
use crate::pki::x509::{parse_cert, pem_decode_all, pem_encode, verify_cert_signature};
use crate::store::prelude::*;

/// 每個請求的處理上限（長輪詢另加 `wait` 秒）
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// 重放快取的上限（滿時 503 `enroll_busy`，不淘汰未滿 10 分鐘的項目）
pub const MAX_REPLAY_ENTRIES: i64 = 100_000;
/// 異常旗標 `burst_from_ip`：10 分鐘內 50 筆
pub const BURST_WINDOW_MS: i64 = 10 * 60_000;
pub const BURST_COUNT: i64 = 50;
/// L10：同一筆申請可連續輪詢 3 次
pub const POLL_BURST: f64 = 3.0;

/// 一條連線的資訊
#[derive(Debug, Clone)]
pub struct ConnInfo {
    pub peer: SocketAddr,
    /// L3 軟上限：這條連線的請求一律 429（`Retry-After`）
    pub soft_limited: Option<Duration>,
}

/// 請求共用的上下文
struct Ctx<'a> {
    svc: &'a Arc<EnrollService>,
    /// 速率限制的來源（IPv6 以 /64 合併）
    src: IpAddr,
    /// 回應中的 `observed_ip`
    observed: String,
    /// 收到請求標頭的時間（時鐘檢查的基準）
    received: i64,
    started: Instant,
}

impl Ctx<'_> {
    fn max_wait(&self) -> Duration {
        Duration::from_secs(u64::from(self.svc.config.enroll.retry_after_max_secs))
    }

    fn limited(&self, code: EnrollErrorCode, scope: RateScope, wait: Duration) -> EnrollError {
        EnrollError::limited(code, scope, wait, &self.observed)
    }

    /// L7 封鎖中
    fn check_blocked(&self, class: InvalidClass) -> Result<(), EnrollError> {
        if let Some(ms) = self.svc.limits.l7.blocked(self.src, class, self.received) {
            self.svc.stats.blocked.fetch_add(1, Ordering::Relaxed);
            let mut e = self.limited(
                EnrollErrorCode::SourceBlocked,
                RateScope::Blocked,
                Duration::from_millis(ms.max(1) as u64),
            );
            // 封鎖的 Retry-After 為剩餘時間，不受 retry_after_max_secs 限制
            let secs = (ms as u64).div_ceil(1000).max(1);
            e.retry_after = Some(secs);
            e.body.retry_after_ms = Some(secs * 1000);
            return Err(e);
        }
        Ok(())
    }

    /// L4：每個來源的新註冊類請求
    fn check_l4(&self) -> Result<(), EnrollError> {
        let c = &self.svc.config.enroll;
        let m = self.svc.multiplier(self.src);
        self.svc
            .limits
            .l4
            .take(
                &self.src,
                f64::from(c.max_requests_per_ip_burst) * m,
                f64::from(c.max_requests_per_ip_per_minute) / 60.0 * m,
                Instant::now(),
                self.max_wait(),
            )
            .map_err(|w| {
                self.svc
                    .stats
                    .limited_enroll
                    .fetch_add(1, Ordering::Relaxed);
                self.limited(EnrollErrorCode::RateLimited, RateScope::Ip, w)
            })
    }

    /// L4r：每個來源的過期續簽
    fn check_l4r(&self) -> Result<(), EnrollError> {
        let c = &self.svc.config.enroll;
        let m = self.svc.multiplier(self.src);
        self.svc
            .limits
            .l4r
            .take(
                &self.src,
                f64::from(c.max_renewals_per_ip_burst) * m,
                f64::from(c.max_renewals_per_ip_per_minute) / 60.0 * m,
                Instant::now(),
                self.max_wait(),
            )
            .map_err(|w| {
                self.svc.stats.limited_renew.fetch_add(1, Ordering::Relaxed);
                self.limited(EnrollErrorCode::RateLimited, RateScope::Ip, w)
            })
    }

    /// L8：全域申請速率
    fn check_l8(&self) -> Result<(), EnrollError> {
        let mut b = self.svc.limits.l8.lock().unwrap_or_else(|e| e.into_inner());
        b.take(Instant::now(), self.max_wait()).map_err(|w| {
            self.svc.stats.busy.fetch_add(1, Ordering::Relaxed);
            self.limited(EnrollErrorCode::EnrollBusy, RateScope::Global, w)
        })
    }

    /// L10：每筆申請
    fn check_l10(&self, request_id: &str) -> Result<(), EnrollError> {
        let interval = f64::from(self.svc.config.enroll.poll_min_interval_secs);
        self.svc
            .limits
            .l10
            .take(
                &request_id.to_string(),
                POLL_BURST,
                1.0 / interval,
                Instant::now(),
                self.max_wait(),
            )
            .map_err(|w| self.limited(EnrollErrorCode::RateLimited, RateScope::Request, w))
    }

    fn mode(&self, class: EndpointClass) -> ModeDecision {
        mode_decision(self.svc.effective().mode, self.svc.is_paused(), class)
    }

    fn require_mode(&self, class: EndpointClass) -> Result<(), EnrollError> {
        match self.mode(class) {
            ModeDecision::Accept => Ok(()),
            ModeDecision::NotAccepted => Err(EnrollError::new(
                EnrollErrorCode::EnrollNotAccepted,
                "new enrollments are not accepted",
            )),
            ModeDecision::NotListening => Err(shutting_down()),
        }
    }

    /// 時鐘檢查（新註冊與過期續簽的第 5 步）：被拒時計入統計與 `KRS-W8302` 彙總，不計入 L5～L7
    fn check_clock(
        &self,
        sent_at: &str,
        sent_at_ms: i64,
        agent_id: &str,
        token_id: Option<&str>,
    ) -> Result<(), EnrollError> {
        let offset = clock_offset_ms(sent_at_ms, self.received);
        if !exceeds_clock_skew(offset) {
            return Ok(());
        }
        self.svc
            .stats
            .clock_rejected
            .fetch_add(1, Ordering::Relaxed);
        self.svc
            .stats
            .clock_since_log
            .fetch_add(1, Ordering::Relaxed);
        self.svc
            .clock_rejected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                agent_id.to_string(),
                (token_id.map(str::to_string), self.received),
            );
        let mut e = EnrollError::new(
            EnrollErrorCode::ClockSkew,
            "client clock differs from server by more than 300 s",
        );
        e.body.client_time = Some(sent_at.to_string());
        e.body.offset_ms = Some(offset);
        e.body.max_skew_ms = Some(kairos_proto::enroll::MAX_CLOCK_SKEW_MS);
        Err(e)
    }

    /// 動態 `poll_after_ms`：max(基準, 同一來源待核准筆數 ÷ 目標速率)，上限
    async fn poll_after_ms(&self, source_ip: &str) -> u64 {
        let c = &self.svc.config.enroll;
        let base = u64::from(c.poll_after_base_secs) * 1000;
        let max = u64::from(c.poll_after_max_secs) * 1000;
        let ip = source_ip.to_string();
        let pending = self
            .svc
            .db
            .read(move |conn| Ok(conn.pending_count_from(&ip)?))
            .await
            .unwrap_or(0)
            .max(0) as u64;
        let dynamic = pending * 1000 / u64::from(c.poll_target_per_ip_per_sec.max(1));
        base.max(dynamic).min(max)
    }
}

fn shutting_down() -> EnrollError {
    let mut e = EnrollError::new(
        EnrollErrorCode::ShuttingDown,
        "enrollment endpoint is closing",
    );
    e.body.retry_after_ms = Some(5000);
    e.retry_after = Some(5);
    e
}

fn db_err(e: DbError) -> EnrollError {
    warn!("註冊端點：資料庫無法讀寫：{e}");
    EnrollError::storage()
}

fn field_err(e: kairos_proto::enroll::FieldError) -> EnrollError {
    EnrollError::bad_request(e.field, e.reason)
}

/// 處理一個請求（hyper 的 service）
pub async fn handle(
    svc: Arc<EnrollService>,
    conn: Arc<ConnInfo>,
    req: Request<Incoming>,
) -> Response<ApiBody> {
    let received = svc.now_ms();
    svc.stats.requests.fetch_add(1, Ordering::Relaxed);
    let peer_ip = conn.peer.ip().to_canonical();
    let ctx = Ctx {
        svc: &svc,
        src: source_key(peer_ip),
        observed: peer_ip.to_string(),
        received,
        started: Instant::now(),
    };
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let result = route(&ctx, &conn, req).await;
    let now = svc.now_ms();
    let resp = match result {
        Ok(r) => r,
        Err(e) => e.into_response(now),
    };
    debug!(
        "註冊端點 {} {} {} → {}（{:.1} ms）",
        conn.peer.ip(),
        method,
        path,
        resp.status().as_u16(),
        ctx.started.elapsed().as_secs_f64() * 1000.0
    );
    resp
}

async fn route(
    ctx: &Ctx<'_>,
    conn: &ConnInfo,
    req: Request<Incoming>,
) -> Result<Response<ApiBody>, EnrollError> {
    if ctx.svc.shutdown.started_at().is_some() {
        return Err(shutting_down());
    }
    let route = match parse_route(req.method().as_str(), req.uri().path()) {
        Ok(r) => r,
        Err(RouteError::NotFound) => return Err(EnrollError::not_found()),
        Err(RouteError::MethodNotAllowed(allow)) => {
            let mut e = EnrollError::new(EnrollErrorCode::MethodNotAllowed, "method not allowed");
            e.allow = Some(allow);
            return Err(e);
        }
    };
    // L3 軟上限：這條連線超過每個來源的新連線速率，請求一律 429
    if let Some(wait) = conn.soft_limited {
        ctx.svc.stats.limited_enroll.fetch_add(1, Ordering::Relaxed);
        return Err(ctx.limited(EnrollErrorCode::RateLimited, RateScope::Ip, wait));
    }
    let max_body = ctx.svc.config.enroll.max_body_bytes;
    let base = REQUEST_TIMEOUT;
    match route {
        Route::ServerInfo => {
            h::reject_body(req.headers())?;
            server_info(ctx)
        }
        Route::Download(name) => {
            h::reject_body(req.headers())?;
            match ctx.mode(EndpointClass::Download) {
                ModeDecision::Accept => {
                    let Some(dir) = ctx.svc.download_dir() else {
                        return Err(EnrollError::not_found());
                    };
                    // L11：每個來源（批次部署來源乘以倍率）與全部的同時下載數
                    let ip = conn.peer.ip().to_canonical();
                    let c = &ctx.svc.config.enroll;
                    let per_ip =
                        (f64::from(c.max_downloads_per_ip) * ctx.svc.multiplier(ip)) as usize;
                    let Some(slot) =
                        ctx.svc
                            .download_slots
                            .take(ip, per_ip.max(1), c.max_downloads as usize)
                    else {
                        ctx.svc.stats.limited_enroll.fetch_add(1, Ordering::Relaxed);
                        return Err(ctx.limited(
                            EnrollErrorCode::RateLimited,
                            RateScope::Download,
                            super::downloads::RETRY_AFTER,
                        ));
                    };
                    let head = req.method() == hyper::Method::HEAD;
                    let inm = req
                        .headers()
                        .get(hyper::header::IF_NONE_MATCH)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    super::downloads::respond(&dir, &name, head, inm.as_deref(), slot)
                        .await
                        .ok_or_else(EnrollError::not_found)
                }
                _ => Ok(h::text(
                    StatusCode::FORBIDDEN,
                    "Kairos 目前不受理新註冊（註冊端點為僅續簽，或新註冊已自動暫停），因此不提供安裝檔下載。\nKairos is not accepting new enrollments; downloads are unavailable.\n",
                )),
            }
        }
        Route::TokenCheck => {
            ctx.check_blocked(InvalidClass::Enroll)?;
            ctx.check_l4()?;
            ctx.require_mode(EndpointClass::TokenCheck)?;
            let body = h::read_json_body(req, max_body).await?;
            timed(base, token_check(ctx, &body)).await
        }
        Route::CreateRequest => {
            ctx.svc
                .stats
                .enroll_requests
                .fetch_add(1, Ordering::Relaxed);
            ctx.check_blocked(InvalidClass::Enroll)?;
            ctx.check_l4()?;
            ctx.require_mode(EndpointClass::NewEnrollment)?;
            let body = h::read_json_body(req, max_body).await?;
            timed(base, create_request(ctx, &body)).await
        }
        Route::CreateRenewal => {
            ctx.svc.stats.renew_requests.fetch_add(1, Ordering::Relaxed);
            ctx.check_blocked(InvalidClass::Renew)?;
            ctx.check_l4r()?;
            ctx.require_mode(EndpointClass::GraceRenewal)?;
            let body = h::read_json_body(req, max_body).await?;
            timed(base, grace_renew(ctx, &body)).await
        }
        Route::Poll(id) => {
            h::reject_body(req.headers())?;
            let wait = parse_wait_secs(req.uri().query())
                .ok_or_else(|| EnrollError::bad_request("wait", "wait must be 0..=60 seconds"))?
                .min(ctx.svc.config.enroll.long_poll_secs);
            let auth = req
                .headers()
                .get(hyper::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            timed(
                base + Duration::from_secs(u64::from(wait)),
                poll(ctx, &id, auth.as_deref(), wait),
            )
            .await
        }
        Route::WithdrawChallenge(id) => {
            ctx.require_mode(EndpointClass::Withdraw)?;
            let _ = h::read_json_body(req, max_body).await?;
            timed(base, withdraw_challenge(ctx, &id)).await
        }
        Route::Withdraw(id) => {
            ctx.require_mode(EndpointClass::Withdraw)?;
            let body = h::read_json_body(req, max_body).await?;
            timed(base, withdraw(ctx, &id, &body)).await
        }
    }
}

/// 處理時限（逾時回 503 `storage_unavailable`：多半是資料庫的寫入鎖等太久）
async fn timed<F>(limit: Duration, f: F) -> Result<Response<ApiBody>, EnrollError>
where
    F: std::future::Future<Output = Result<Response<ApiBody>, EnrollError>>,
{
    match tokio::time::timeout(limit, f).await {
        Ok(r) => r,
        Err(_) => {
            warn!("註冊端點的請求超過 {} 秒未完成", limit.as_secs());
            Err(EnrollError::storage())
        }
    }
}

// ---------------------------------------------------------------------------
// server-info、token-check
// ---------------------------------------------------------------------------

fn server_info(ctx: &Ctx<'_>) -> Result<Response<ApiBody>, EnrollError> {
    ctx.check_l4()?;
    let eff = ctx.svc.effective();
    let state = ctx.svc.mode_state();
    let mut enroll_features = vec![
        ENROLL_FEATURE_WITHDRAW.to_string(),
        ENROLL_FEATURE_GRACE_RENEW.to_string(),
    ];
    if ctx.svc.config.enroll.long_poll_secs > 0 {
        enroll_features.push(ENROLL_FEATURE_LONG_POLL.to_string());
    }
    let mut features: Vec<String> = kairos_proto::SERVER_FEATURES
        .iter()
        .map(|f| f.to_string())
        .collect();
    features.extend(ctx.svc.agent_features().iter().map(|f| f.to_string()));
    let info = ServerInfo {
        server_time: format_rfc3339_ms(ctx.received),
        kairos_version: env!("CARGO_PKG_VERSION").to_string(),
        enroll_api: vec![ENROLL_API_VERSION.to_string()],
        enroll_features,
        proto_min: SUPPORTED_PROTO.min,
        proto_max: SUPPORTED_PROTO.max,
        features,
        accepts: Accepts {
            new_enrollment: state == ModeState::Open,
            grace_renewal: state != ModeState::Closed && eff.policy.expired_renew_grace_days > 0,
        },
        observed_ip: ctx.observed.clone(),
    };
    Ok(h::ok(&info, ctx.svc.now_ms()))
}

/// token 不存在或已停用：403 `token_invalid`，以 token 雜湊計入 L7
async fn token_invalid(ctx: &Ctx<'_>, hash: [u8; 32]) -> EnrollError {
    ctx.svc
        .record_invalid(ctx.src, InvalidClass::Enroll, hash)
        .await;
    EnrollError::new(
        EnrollErrorCode::TokenInvalid,
        "enrollment token is not valid",
    )
}

/// token 的狀態檢查（過期、用完）
fn token_usable(t: &TokenRow, now: i64) -> Result<(), EnrollError> {
    match t.state(now) {
        TokenState::Expired => {
            let mut e = EnrollError::new(
                EnrollErrorCode::TokenExpired,
                "enrollment token has expired",
            );
            e.body.expired_at = Some(format_rfc3339_ms(t.expires_at));
            Err(e)
        }
        TokenState::Exhausted => Err(EnrollError::new(
            EnrollErrorCode::TokenExhausted,
            "enrollment token has no remaining uses",
        )),
        _ => Ok(()),
    }
}

fn source_allowed(t: &TokenRow, ip: IpAddr) -> Option<bool> {
    let cidrs = t.allowed_cidrs.as_ref()?;
    Some(
        crate::per_ip::Allowlist::parse(cidrs)
            .map(|a| a.contains(ip))
            .unwrap_or(false),
    )
}

async fn token_check(ctx: &Ctx<'_>, body: &[u8]) -> Result<Response<ApiBody>, EnrollError> {
    let req: TokenCheckRequest = h::parse_json(body)?;
    let checked = validate_token_check(&req).map_err(field_err)?;
    let hash = checked.token.lookup_hash();
    let row = ctx
        .svc
        .db
        .read(move |conn| Ok(conn.token_by_hash(&hash)?))
        .await
        .map_err(db_err)?;
    let Some(t) = row.filter(|t| t.disabled_at.is_none()) else {
        return Err(token_invalid(ctx, hash).await);
    };
    token_usable(&t, ctx.received)?;
    let peer: IpAddr = ctx.observed.parse().unwrap_or(ctx.src);
    let resp = TokenCheckResponse {
        server_time: String::new(),
        token: TokenStatus {
            name: t.name.clone(),
            approval: t.approval,
            expires_at: format_rfc3339_ms(t.expires_at),
            remaining_uses: t.remaining_uses(),
            default_tags: t.tags.clone(),
            source_allowed: source_allowed(&t, peer),
        },
        observed_ip: ctx.observed.clone(),
    };
    Ok(h::ok(&resp, ctx.svc.now_ms()))
}

// ---------------------------------------------------------------------------
// 回應的組成
// ---------------------------------------------------------------------------

/// 申請目前的結果（新註冊、過期續簽、輪詢共用）
fn result_of(
    svc: &EnrollService,
    r: &RequestRow,
    cert: Option<&CertRow>,
    poll_after_ms: Option<u64>,
    observed_ip: Option<String>,
    now: i64,
) -> Result<EnrollResult, EnrollError> {
    let (state, detail) = r.state.protocol();
    let pending = r.state.is_pending();
    let issued = r.state == DbState::Issued;
    let certificate = if issued {
        let c = cert.ok_or_else(|| {
            warn!(
                "申請 {} 已簽發但找不到憑證 {:?}",
                r.request_id, r.issued_serial
            );
            EnrollError::storage()
        })?;
        let ca = svc.pki.server_ca().ok_or_else(EnrollError::storage)?;
        Some(IssuedCertificate {
            serial: c.serial.clone(),
            not_before: format_rfc3339_ms(c.not_before),
            not_after: format_rfc3339_ms(c.not_after),
            client_cert_pem: pem_encode("CERTIFICATE", &c.cert_der),
            chain_pem: String::new(),
            ca_pem: ca.pem,
        })
    } else {
        None
    };
    let decided = matches!(r.state, DbState::Issued | DbState::Rejected);
    Ok(EnrollResult {
        server_time: format_rfc3339_ms(now),
        request_id: r.request_id.clone(),
        request_code: RequestCode::parse(&r.request_code).ok_or_else(EnrollError::storage)?,
        kind: r.kind.protocol(),
        state,
        agent_id: r.agent_id.clone(),
        expires_at: pending.then(|| format_rfc3339_ms(r.expires_at)),
        poll_after_ms: if pending { poll_after_ms } else { None },
        approval_detail: if pending { detail } else { None },
        certificate,
        policy: issued.then(|| svc.effective().protocol_policy()),
        decided_at: if decided {
            r.decided_at.map(format_rfc3339_ms)
        } else {
            None
        },
        decided_by: if decided { r.decided_by.clone() } else { None },
        reject_reason: if r.state == DbState::Rejected {
            r.reject_reason.clone()
        } else {
            None
        },
        observed_ip,
    })
}

/// 讀出申請與已簽發的憑證
async fn load_request(
    svc: &Arc<EnrollService>,
    request_id: &str,
) -> Result<Option<(RequestRow, Option<CertRow>)>, EnrollError> {
    let id = request_id.to_string();
    svc.db
        .read(move |conn| {
            let Some(r) = conn.get_request(&id)? else {
                return Ok(None);
            };
            let cert = match &r.issued_serial {
                Some(s) => conn.get_cert(s)?,
                None => None,
            };
            Ok(Some((r, cert)))
        })
        .await
        .map_err(db_err)
}

async fn respond_with(
    ctx: &Ctx<'_>,
    r: &RequestRow,
    cert: Option<&CertRow>,
) -> Result<Response<ApiBody>, EnrollError> {
    let poll_after = if r.state.is_pending() {
        Some(ctx.poll_after_ms(&r.source_ip).await)
    } else {
        None
    };
    let result = result_of(
        ctx.svc,
        r,
        cert,
        poll_after,
        Some(ctx.observed.clone()),
        ctx.svc.now_ms(),
    )?;
    Ok(h::ok(&result, ctx.svc.now_ms()))
}

// ---------------------------------------------------------------------------
// 新註冊
// ---------------------------------------------------------------------------

/// 新註冊建立申請的結果
enum Created {
    /// 冪等：同一個申請
    Existing(Box<(RequestRow, Option<CertRow>)>),
    New(Box<RequestRow>, bool),
}

async fn create_request(ctx: &Ctx<'_>, body: &[u8]) -> Result<Response<ApiBody>, EnrollError> {
    let svc = ctx.svc;
    let mut req: EnrollRequest = h::parse_json(body)?;
    // 4. 本文
    let checked: CheckedEnrollRequest = validate_enroll_request(&req).map_err(field_err)?;
    // 個資政策（privacy.hostname）：申請、清冊與日誌只保存假名（kairos/PRIVACY.md）
    req.hostname = svc.privacy.hostname(&req.hostname);
    req.display_name = svc.privacy.display_name(req.display_name.take());
    // 5. 時鐘（不消耗 token、不建立申請、不計入 L5～L7）
    ctx.check_clock(&req.sent_at, checked.sent_at_ms, &checked.agent_id, None)?;
    // 6. token
    let hash = checked.token.lookup_hash();
    let agent_id = checked.agent_id.clone();
    let csr_sha = sha256(&checked.csr_der);
    let (token, existing) = svc
        .db
        .read(move |conn| {
            let t = conn.token_by_hash(&hash)?;
            let existing = conn.request_by_csr(&agent_id, &csr_sha)?;
            let cert = match existing.as_ref().and_then(|r| r.issued_serial.clone()) {
                Some(s) => conn.get_cert(&s)?,
                None => None,
            };
            Ok((t, existing.map(|r| (r, cert))))
        })
        .await
        .map_err(db_err)?;
    let Some(token) = token.filter(|t| t.disabled_at.is_none()) else {
        return Err(token_invalid(ctx, hash).await);
    };
    let peer: IpAddr = ctx.observed.parse().unwrap_or(ctx.src);
    let invalid = || {
        EnrollError::new(
            EnrollErrorCode::TokenInvalid,
            "enrollment token is not valid",
        )
    };
    if token
        .allowed_agent_ids
        .as_ref()
        .is_some_and(|ids| !ids.contains(&checked.agent_id))
    {
        return Err(invalid());
    }
    let in_cidrs = source_allowed(&token, peer);
    if token.approval == TokenApproval::Auto && in_cidrs == Some(false) {
        return Err(invalid());
    }
    // 6′. 過期、用完
    token_usable(&token, ctx.received)?;
    // 7. 冪等
    let poll_hash = sha256(&checked.poll_secret);
    if let Some((r, cert)) = existing {
        return idempotent(ctx, &r, cert.as_ref(), &poll_hash).await;
    }
    // 8. L6 每 agent_id
    let c = &svc.config.enroll;
    let now_i = Instant::now();
    let note_queued = || {
        svc.queued.lock().unwrap_or_else(|e| e.into_inner()).insert(
            checked.agent_id.clone(),
            (token.token_id.clone(), ctx.received),
        );
    };
    if let Err(w) = svc.limits.l6.take(
        &checked.agent_id,
        f64::from(c.max_requests_per_agent_burst),
        f64::from(c.max_requests_per_agent_per_minute) / 60.0,
        now_i,
        ctx.max_wait(),
    ) {
        svc.stats.limited_enroll.fetch_add(1, Ordering::Relaxed);
        note_queued();
        return Err(ctx.limited(EnrollErrorCode::RateLimited, RateScope::Agent, w));
    }
    // 9. L5 每 token、L5b 網段外（需核准的 token）
    let drain = if token.batch_mode {
        60
    } else {
        c.token_min_drain_secs
    };
    if drain > 0 {
        let rate = (token.max_uses as f64 / f64::from(drain)).max(1.0 / 60.0);
        let burst = f64::from(c.max_requests_per_token_burst).min(token.max_uses as f64);
        if let Err(w) = svc
            .limits
            .l5
            .take(&token.token_id, burst, rate, now_i, ctx.max_wait())
        {
            svc.stats.limited_enroll.fetch_add(1, Ordering::Relaxed);
            note_queued();
            return Err(ctx.limited(EnrollErrorCode::RateLimited, RateScope::Token, w));
        }
    }
    if in_cidrs == Some(false) {
        let per_hour = f64::from(c.max_outside_subnet_requests_per_token_per_hour);
        if per_hour <= 0.0 {
            return Err(ctx.limited(
                EnrollErrorCode::RateLimited,
                RateScope::Token,
                ctx.max_wait(),
            ));
        }
        if let Err(w) = svc.limits.l5b.take(
            &token.token_id,
            per_hour,
            per_hour / 3600.0,
            now_i,
            ctx.max_wait(),
        ) {
            svc.stats.limited_enroll.fetch_add(1, Ordering::Relaxed);
            return Err(ctx.limited(EnrollErrorCode::RateLimited, RateScope::Token, w));
        }
    }
    svc.queued
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&checked.agent_id);
    // 10. CSR
    let csr = check_csr(&checked.csr_der, &checked.agent_id).map_err(|m| {
        let mut e = EnrollError::new(EnrollErrorCode::InvalidCsr, m);
        e.body.field = Some("csr".into());
        e
    })?;
    // 11. L8
    ctx.check_l8()?;
    // 12～14：交易中再確認冪等、token 次數、L9，建立申請
    let s = svc.clone();
    let token_id = token.token_id.clone();
    let source_ip = ctx.observed.clone();
    let req2 = req.clone();
    let checked2 = checked.clone();
    let csr2 = csr.clone();
    let now = ctx.received;
    let batch_source = svc.is_batch_source(peer);
    let (s2, req3, checked3, csr3, token2, source2) = (
        svc.clone(),
        req.clone(),
        checked.clone(),
        csr.clone(),
        token.clone(),
        ctx.observed.clone(),
    );
    let pre = svc
        .db
        .read(move |conn| {
            pre_assess(
                &s2,
                conn,
                &req3,
                &checked3,
                &csr3,
                &token2,
                in_cidrs,
                batch_source,
                &source2,
                now,
            )
        })
        .await
        .map_err(db_err)??;
    let created = svc
        .db
        .write(move |conn| {
            create_in_tx(
                &s, conn, &req2, &checked2, &csr2, &token_id, &pre, &source_ip, poll_hash, now,
            )
        })
        .await
        .map_err(db_err)?;
    let (row, auto) = match created {
        Ok(Created::Existing(b)) => {
            let (r, cert) = *b;
            return idempotent(ctx, &r, cert.as_ref(), &poll_hash).await;
        }
        Ok(Created::New(r, auto)) => (*r, auto),
        Err(e) => return Err(e),
    };
    // 15. 自動核准：線上簽發
    if auto {
        match issue_now(svc, &row.request_id, "auto").await {
            Ok(Some((r, cert))) => {
                info!(
                    "註冊申請 {}（{}，agent_id {}，來源 {}）以自動核准的 token 簽發",
                    row.request_code, row.hostname, row.agent_id, row.source_ip
                );
                return respond_with(ctx, &r, cert.as_ref()).await;
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    } else {
        svc.stats.pending_created.fetch_add(1, Ordering::Relaxed);
        info!(
            "資訊 KRS-I3307：新的註冊申請待核准：{}（{}，agent_id {}，來源 {}，token {}{}）",
            format_code(&row.request_code),
            row.hostname.escape_debug(),
            row.agent_id,
            row.source_ip,
            row.token_id.as_deref().unwrap_or("-"),
            if row.flags.is_empty() {
                String::new()
            } else {
                format!("，異常旗標 {}", row.flags.join("、"))
            }
        );
    }
    respond_with(ctx, &row, None).await
}

fn format_code(compact: &str) -> String {
    RequestCode::parse(compact)
        .map(|c| c.to_string())
        .unwrap_or_else(|| compact.to_string())
}

/// 冪等：同一 `agent_id`＋同一 CSR 已有申請。`poll_secret` 相同時回傳目前的狀態，不同時 409
async fn idempotent(
    ctx: &Ctx<'_>,
    r: &RequestRow,
    cert: Option<&CertRow>,
    poll_hash: &[u8; 32],
) -> Result<Response<ApiBody>, EnrollError> {
    let same = r
        .poll_secret_hash
        .as_deref()
        .is_some_and(|h| h.ct_eq(poll_hash).into());
    if !same {
        return Err(EnrollError::new(
            EnrollErrorCode::RequestConflict,
            "a request for this agent_id and CSR already exists with a different poll_secret",
        ));
    }
    respond_with(ctx, r, cert).await
}

/// 交易前的判定（唯讀連線；不佔用寫入鎖）：公鑰重複、異常旗標、自動核准與否、L9 待核准佇列、授權
struct Pre {
    flags: Vec<String>,
    auto: bool,
}

#[allow(clippy::too_many_arguments)]
fn pre_assess(
    svc: &EnrollService,
    conn: &dyn Store,
    req: &EnrollRequest,
    checked: &CheckedEnrollRequest,
    csr: &crate::pki::csr::CheckedCsr,
    token: &TokenRow,
    in_cidrs: Option<bool>,
    batch_source: bool,
    source_ip: &str,
    now: i64,
) -> Result<Result<Pre, EnrollError>, DbError> {
    // 公鑰不得是這個 agent_id 先前任何一張憑證的公鑰
    if conn.spki_used_by_agent(&checked.agent_id, &csr.spki_sha256)? {
        let mut e = EnrollError::new(
            EnrollErrorCode::InvalidCsr,
            "csr public key was already used by a previous certificate of this agent_id",
        );
        e.body.field = Some("csr".into());
        return Ok(Err(e));
    }
    // 異常旗標（S13）
    let host = conn.host_enroll(&checked.agent_id)?;
    let certs = conn.certs_of_agent(&checked.agent_id)?;
    // 已有有效憑證：Kairos 簽發的憑證未撤銷、未過期；或清冊中有這台主機、使用手動簽發的憑證
    // （Kairos 不知道它的效期，視為有效）
    let has_valid_cert = certs
        .iter()
        .any(|c| c.revoked_at.is_none() && c.not_after > now)
        || host
            .as_ref()
            .is_some_and(|h| h.lifecycle == "active" && h.current_serial.is_none());
    let mut flags = Vec::new();
    if in_cidrs == Some(false) {
        flags.push("source_outside_cidrs".to_string());
    }
    if conn.hostname_in_use(&req.hostname, &checked.agent_id)? {
        flags.push("duplicate_hostname".to_string());
    }
    if has_valid_cert {
        flags.push("agent_id_exists".to_string());
    }
    if !batch_source
        && in_cidrs != Some(true)
        && conn.recent_count_from(source_ip, now - BURST_WINDOW_MS)? + 1 >= BURST_COUNT
    {
        flags.push("burst_from_ip".to_string());
    }
    if req.server_verification == ServerVerification::Tofu {
        flags.push("unverified_server".to_string());
    }
    if req.previous_serial.is_some() || certs.iter().any(|c| c.revoked_at.is_some()) {
        flags.push("reenroll_after_revocation".to_string());
    }
    if let (Some(h), Some(fp)) = (&host, &req.host_fingerprint)
        && h.host_fingerprint
            .as_deref()
            .is_some_and(|known| known != fp)
    {
        flags.push("fingerprint_mismatch".to_string());
    }
    // 自動核准：token 免審核、來源在網段內，且沒有已存在的有效憑證（避免以 token 取得另一台主機的身分）
    let auto = token.approval == TokenApproval::Auto && !has_valid_cert;
    // 12. L9 待核准佇列（結果會是待核准、或已核准待簽發時）
    let online = matches!(svc.pki.status(), super::pki_mgr::IssueStatus::Online { .. });
    let will_be_pending = !auto || !online;
    if will_be_pending && conn.pending_count()? >= i64::from(svc.config.enroll.max_pending) {
        let mut e = EnrollError::limited(
            EnrollErrorCode::PendingQueueFull,
            RateScope::Global,
            Duration::from_secs(3600),
            source_ip,
        );
        e.retry_after = Some(3600);
        e.body.retry_after_ms = Some(3_600_000);
        return Ok(Err(e));
    }
    // 13. 授權節點數（本版沒有授權機制）
    if auto && !svc.license_allows(1) {
        return Ok(Err(EnrollError::new(
            EnrollErrorCode::LicenseExhausted,
            "license node count would be exceeded",
        )));
    }
    Ok(Ok(Pre { flags, auto }))
}

/// 新註冊的建立（交易內）。外層 `Result` 為資料庫錯誤，內層為協定錯誤
#[allow(clippy::too_many_arguments)]
fn create_in_tx(
    svc: &EnrollService,
    conn: &dyn Store,
    req: &EnrollRequest,
    checked: &CheckedEnrollRequest,
    csr: &crate::pki::csr::CheckedCsr,
    token_id: &str,
    pre: &Pre,
    source_ip: &str,
    poll_hash: [u8; 32],
    now: i64,
) -> Result<Result<Created, EnrollError>, DbError> {
    let tx = conn.begin(TxMode::Immediate)?;
    // 冪等（並行的重送）
    if let Some(r) = tx.request_by_csr(&checked.agent_id, &csr.csr_sha256)? {
        let cert = match &r.issued_serial {
            Some(s) => tx.get_cert(s)?,
            None => None,
        };
        return Ok(Ok(Created::Existing(Box::new((r, cert)))));
    }
    let Some(token) = tx.get_token(token_id)? else {
        return Ok(Err(EnrollError::new(
            EnrollErrorCode::TokenInvalid,
            "enrollment token is not valid",
        )));
    };
    if token.disabled_at.is_some() {
        return Ok(Err(EnrollError::new(
            EnrollErrorCode::TokenInvalid,
            "enrollment token is not valid",
        )));
    }
    if let Err(e) = token_usable(&token, now) {
        return Ok(Err(e));
    }
    let flags = pre.flags.clone();
    let auto = pre.auto && token.approval == TokenApproval::Auto;
    // 14. 建立申請
    let request_id = new_uuid()?;
    let code = request_code_from_spki(&csr.spki_der);
    let os = req
        .os
        .as_ref()
        .map(|o| serde_json::to_string(o).unwrap_or_default());
    let row = RequestRow {
        request_id: request_id.clone(),
        kind: DbKind::Enroll,
        agent_id: checked.agent_id.clone(),
        request_code: code.compact(),
        state: if auto {
            DbState::Approved
        } else {
            DbState::PendingApproval
        },
        csr_der: checked.csr_der.clone(),
        csr_sha256: csr.csr_sha256.to_vec(),
        spki_sha256: csr.spki_sha256.to_vec(),
        poll_secret_hash: Some(poll_hash.to_vec()),
        token_id: Some(token.token_id.clone()),
        expired_serial: None,
        previous_serial: req.previous_serial.clone(),
        source_ip: source_ip.to_string(),
        hostname: req.hostname.clone(),
        display_name: req.display_name.clone(),
        tags: req.tags.clone(),
        os,
        host_fingerprint: req.host_fingerprint.clone(),
        pulse_version: Some(req.pulse_version.clone()),
        server_verification: Some(
            match req.server_verification {
                ServerVerification::CaPin => "ca_pin",
                ServerVerification::CaFile => "ca_file",
                ServerVerification::Tofu => "tofu",
            }
            .to_string(),
        ),
        flags,
        cert_lifetime_days: token.cert_lifetime_days,
        created_at: now,
        expires_at: now + i64::from(svc.config.enroll.request_expiry_days) * 86_400_000,
        decided_at: auto.then_some(now),
        decided_by: auto.then(|| "auto".to_string()),
        reject_reason_code: None,
        reject_reason: None,
        issued_serial: None,
        issue_error: None,
        delivered_at: None,
        withdrawn_at: None,
        withdraw_reason: None,
        withdraw_initiated_by: None,
        withdraw_ip: None,
        closed_at: None,
    };
    tx.insert_request(&row)?;
    tx.consume_token(&token.token_id, now)?;
    let replaced = tx.replace_pending_for_agent(&row.agent_id, &request_id, now)?;
    tx.audit(
        now,
        &Audit {
            actor: &format!("host:{}", &row.agent_id[..8]),
            actor_ip: Some(source_ip),
            category: "enroll",
            action: "request.create",
            object: Some(&request_id),
            result: "ok",
            code: (!auto).then_some("KRS-I3307"),
            details: Some(json!({
                "agent_id": row.agent_id,
                "hostname": row.hostname,
                "token_id": row.token_id,
                "auto": auto,
                "flags": row.flags,
                "replaced": replaced,
                "server_verification": row.server_verification,
            })),
        },
    )?;
    tx.commit()?;
    for id in &replaced {
        svc.notify_request(id);
    }
    Ok(Ok(Created::New(Box::new(row), auto)))
}

fn new_uuid() -> Result<String, DbError> {
    let mut b = [0u8; 16];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| DbError("無法取得隨機數".into()))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// 立即簽發（自動核准、過期續簽、主控台核准）。回傳簽發後的申請與憑證；線上簽發不可用時 `None`
/// （申請停在「已核准，待簽發」）
pub async fn issue_now(
    svc: &Arc<EnrollService>,
    request_id: &str,
    decided_by: &str,
) -> Result<Option<(RequestRow, Option<CertRow>)>, EnrollError> {
    let s = svc.clone();
    let id = request_id.to_string();
    let by = decided_by.to_string();
    let eff = svc.effective();
    let now = svc.now_ms();
    let outcome = svc
        .db
        .write(move |conn| Ok(s.pki.issue(conn, &id, &by, &eff, now)))
        .await
        .map_err(db_err)?;
    match outcome {
        Ok(IssueOutcome::Issued(c)) => {
            svc.stats.issued.fetch_add(1, Ordering::Relaxed);
            svc.set_decommissioned(&c.agent_id, false);
            svc.notify_request(request_id);
        }
        Ok(_) => {}
        Err(IssueError::Unavailable(r)) => {
            debug!("申請 {request_id} 已核准，等待簽發：{r}");
            return Ok(None);
        }
        Err(IssueError::Failed(r)) => {
            warn!("申請 {request_id} 已核准但簽發失敗（停在「已核准，待簽發」）：{r}");
            return Ok(None);
        }
        Err(IssueError::Db(e)) => return Err(db_err(e)),
    }
    load_request(svc, request_id).await
}

// ---------------------------------------------------------------------------
// 輪詢
// ---------------------------------------------------------------------------

async fn poll(
    ctx: &Ctx<'_>,
    request_id: &str,
    auth: Option<&str>,
    wait: u32,
) -> Result<Response<ApiBody>, EnrollError> {
    let svc = ctx.svc;
    let Some(secret) = auth.and_then(parse_poll_authorization) else {
        // 缺少或格式錯誤：與申請不存在相同（不透露是否存在），以出示的值的雜湊計入 L7
        svc.record_invalid(
            ctx.src,
            InvalidClass::Enroll,
            sha256(auth.unwrap_or("").as_bytes()),
        )
        .await;
        return Err(EnrollError::request_not_found());
    };
    let presented = sha256(&secret);
    let Some((r, cert)) = load_request(svc, request_id).await? else {
        svc.record_invalid(ctx.src, InvalidClass::Enroll, presented)
            .await;
        return Err(EnrollError::request_not_found());
    };
    let expected = r.poll_secret_hash.clone().unwrap_or_else(|| vec![0; 32]);
    let same: bool = expected.ct_eq(&presented[..]).into();
    if !same || r.poll_secret_hash.is_none() {
        let class = if r.kind == DbKind::Enroll {
            InvalidClass::Enroll
        } else {
            InvalidClass::Renew
        };
        svc.record_invalid(ctx.src, class, presented).await;
        return Err(EnrollError::request_not_found());
    }
    ctx.check_l10(&r.request_id)?;
    // 長輪詢：待核准且有名額時，保持連線直到狀態改變或 wait 秒到期
    let (r, cert) = if r.state.is_pending() && wait > 0 {
        let c = &svc.config.enroll;
        match svc.limits.long_polls.try_acquire(
            ctx.src,
            c.max_long_polls_per_ip as usize,
            c.max_long_polls as usize,
        ) {
            Some(_slot) => long_poll(svc, r, Duration::from_secs(u64::from(wait))).await?,
            None => (r, cert),
        }
    } else {
        (r, cert)
    };
    if r.state == DbState::Issued && r.delivered_at.is_none() {
        let id = r.request_id.clone();
        let now = svc.now_ms();
        let _ = svc
            .db
            .write(move |conn| Ok(conn.mark_delivered(&id, now)?))
            .await;
    }
    let mut result = result_of(svc, &r, cert.as_ref(), None, None, svc.now_ms())?;
    if r.state.is_pending() {
        result.poll_after_ms = Some(ctx.poll_after_ms(&r.source_ip).await);
    }
    Ok(h::ok(&result, svc.now_ms()))
}

async fn long_poll(
    svc: &Arc<EnrollService>,
    r: RequestRow,
    wait: Duration,
) -> Result<(RequestRow, Option<CertRow>), EnrollError> {
    let deadline = tokio::time::Instant::now() + wait;
    let notify = svc.watch(&r.request_id, r.state.as_str());
    let initial = r.state;
    let id = r.request_id.clone();
    let result = async {
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            // 登記後再讀一次，避免漏掉登記前的變化
            if let Some((cur, cert)) = load_request(svc, &id).await?
                && cur.state != initial
            {
                return Ok((cur, cert));
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => {
                    return load_request(svc, &id).await?.ok_or_else(EnrollError::request_not_found);
                }
                _ = svc.shutdown.cancelled() => {
                    return load_request(svc, &id).await?.ok_or_else(EnrollError::request_not_found);
                }
            }
        }
    }
    .await;
    svc.unwatch(&id);
    result
}

// ---------------------------------------------------------------------------
// 過期續簽
// ---------------------------------------------------------------------------

/// 信任錨：Kairos 的簽發 CA（`issuing-cas.pem`）與 `auth.client_ca`
fn trust_anchor_ders(svc: &EnrollService) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for path in [
        Some(crate::pki::issuing_cas_path(&svc.data_dir)),
        svc.config
            .auth
            .client_ca_path()
            .map(std::path::PathBuf::from),
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(blocks) = pem_decode_all(&text, Some("CERTIFICATE"))
        {
            out.extend(blocks.into_iter().map(|(_, der)| der));
        }
    }
    out
}

/// 第 7 步：身分（不含主機指紋）。成功時回傳通過的簽章內容
fn verify_grace_identity(
    svc: &EnrollService,
    req: &GraceRenewRequest,
    agent_id: &str,
    csr_der: &[u8],
    cert_der: &[u8],
) -> Result<(crate::pki::x509::CertInfo, Vec<u8>), &'static str> {
    let info = parse_cert(cert_der).map_err(|_| "expired_certificate is not a certificate")?;
    if !trust_anchor_ders(svc)
        .iter()
        .any(|ca| verify_cert_signature(cert_der, ca))
    {
        return Err("certificate is not issued by a trusted CA");
    }
    // 已退役的簽發 CA 簽出的憑證：`auth.client_ca` 直接含有那張簽發 CA 時仍會通過上面的檢查（PKI.md 第 15.4 節）
    if svc
        .pki
        .retired_ca_ders()
        .iter()
        .any(|ca| verify_cert_signature(cert_der, ca))
    {
        return Err("certificate is issued by a retired issuing CA");
    }
    if info.other_sans != 0 || info.dns_sans.len() != 1 || info.dns_sans[0] != agent_id {
        return Err("certificate SAN does not match agent_id");
    }
    if kairos_proto::enroll::p256_point_from_spki(&info.spki_der).is_none() {
        return Err("certificate key is not P-256");
    }
    for name in svc.endpoint_names() {
        let Ok(input) = grace_renew_signing_input(
            agent_id,
            &req.host_fingerprint,
            csr_der,
            &req.sent_at,
            &name,
        ) else {
            continue;
        };
        if verify_ecdsa_p256_sha256(&info.spki_der, &input, &req.signature).is_ok() {
            return Ok((info, input));
        }
    }
    Err("signature does not verify")
}

/// 過期續簽在交易中的判定（第 8～16 步）
enum GraceOutcome {
    Respond(Box<(RequestRow, Option<CertRow>)>),
    /// 建立了申請；`true`：立即簽發
    Created(Box<RequestRow>, bool),
    Error(EnrollError, bool),
}

async fn grace_renew(ctx: &Ctx<'_>, body: &[u8]) -> Result<Response<ApiBody>, EnrollError> {
    let svc = ctx.svc;
    let mut req: GraceRenewRequest = h::parse_json(body)?;
    // 4. 本文
    let checked = validate_grace_renew_request(&req).map_err(field_err)?;
    // 個資政策（privacy.hostname）：簽章不涵蓋主機名稱，驗證後轉換
    req.hostname = svc.privacy.hostname(&req.hostname);
    // 5. 時鐘
    ctx.check_clock(&req.sent_at, checked.sent_at_ms, &checked.agent_id, None)?;
    // 6. L6r
    let c = &svc.config.enroll;
    if let Err(w) = svc.limits.l6r.take(
        &checked.agent_id,
        f64::from(c.max_renewals_per_agent_per_hour),
        f64::from(c.max_renewals_per_agent_per_hour) / 3600.0,
        Instant::now(),
        ctx.max_wait(),
    ) {
        svc.stats.limited_renew.fetch_add(1, Ordering::Relaxed);
        return Err(ctx.limited(EnrollErrorCode::RateLimited, RateScope::Agent, w));
    }
    // 7. 身分
    let mismatch = || {
        EnrollError::new(
            EnrollErrorCode::RenewIdentityMismatch,
            "renewal identity mismatch",
        )
    };
    let (old, signing_input) = match verify_grace_identity(
        svc,
        &req,
        &checked.agent_id,
        &checked.csr_der,
        &checked.expired_certificate_der,
    ) {
        Ok(v) => v,
        Err(why) => {
            debug!(
                "過期續簽的身分不符（agent_id {}，來源 {}）：{why}",
                checked.agent_id, ctx.observed
            );
            svc.record_invalid(
                ctx.src,
                InvalidClass::Renew,
                sha256(checked.agent_id.as_bytes()),
            )
            .await;
            return Err(mismatch());
        }
    };
    let agent_id = checked.agent_id.clone();
    let host = svc
        .db
        .read(move |conn| Ok(conn.host_enroll(&agent_id)?))
        .await
        .map_err(db_err)?;
    let fp_ok = host
        .as_ref()
        .and_then(|h| h.host_fingerprint.as_deref())
        .is_some_and(|fp| fp == req.host_fingerprint);
    if !fp_ok {
        svc.record_invalid(
            ctx.src,
            InvalidClass::Renew,
            sha256(checked.agent_id.as_bytes()),
        )
        .await;
        return Err(mismatch());
    }
    let key = replay_cache_key(&signing_input);
    let eff = svc.effective();
    let s = svc.clone();
    let observed = ctx.observed.clone();
    let now = ctx.received;
    let checked2 = checked.clone();
    let req2 = req.clone();
    let outcome = svc
        .db
        .write(move |conn| grace_in_tx(&s, conn, &req2, &checked2, &old, key, &eff, &observed, now))
        .await
        .map_err(db_err)?;
    match outcome {
        GraceOutcome::Error(e, count_l7) => {
            if count_l7 {
                svc.record_invalid(ctx.src, InvalidClass::Renew, key).await;
            }
            Err(e)
        }
        GraceOutcome::Respond(b) => respond_with(ctx, &b.0, b.1.as_ref()).await,
        GraceOutcome::Created(r, issue) => {
            let r = *r;
            if issue {
                if let Some((r2, cert)) = issue_now(svc, &r.request_id, "system").await? {
                    info!(
                        "過期續簽：已為 {}（agent_id {}）簽發新憑證",
                        r2.hostname.escape_debug(),
                        r2.agent_id
                    );
                    return respond_with(ctx, &r2, cert.as_ref()).await;
                }
            } else {
                info!(
                    "資訊 KRS-I3307：過期續簽申請待核准：{}（{}，agent_id {}）",
                    format_code(&r.request_code),
                    r.hostname.escape_debug(),
                    r.agent_id
                );
            }
            respond_with(ctx, &r, None).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn grace_in_tx(
    svc: &EnrollService,
    conn: &dyn Store,
    req: &GraceRenewRequest,
    checked: &kairos_proto::enroll::CheckedGraceRenewRequest,
    old: &crate::pki::x509::CertInfo,
    key: [u8; 32],
    eff: &crate::settings::Effective,
    source_ip: &str,
    now: i64,
) -> Result<GraceOutcome, DbError> {
    let tx = conn.begin(TxMode::Immediate)?;
    let agent = &checked.agent_id;
    let attempt =
        |code: &str| json!({"at": format_rfc3339_ms(now), "result": code, "ip": source_ip});
    // 8. 重放快取：通過身分驗證的請求一律寫入（不論後面的結果）
    if tx.replay_seen(&key, now)? {
        tx.set_last_grace_attempt(agent, &attempt("replayed_request"))?;
        tx.commit()?;
        return Ok(GraceOutcome::Error(
            EnrollError::new(
                EnrollErrorCode::ReplayedRequest,
                "renewal request has already been received",
            ),
            true,
        ));
    }
    if tx.replay_count(now)? >= MAX_REPLAY_ENTRIES {
        return Ok(GraceOutcome::Error(
            EnrollError::limited(
                EnrollErrorCode::EnrollBusy,
                RateScope::Global,
                Duration::from_secs(60),
                source_ip,
            ),
            false,
        ));
    }
    tx.replay_insert(&key, agent, now, kairos_proto::enroll::REPLAY_CACHE_TTL_MS)?;
    let fail = |tx: Box<dyn Tx + '_>,
                code: EnrollErrorCode,
                message: &str|
     -> Result<GraceOutcome, DbError> {
        tx.set_last_grace_attempt(agent, &attempt(code.as_str()))?;
        tx.commit()?;
        Ok(GraceOutcome::Error(EnrollError::new(code, message), false))
    };
    // 9. 冪等：同一 agent_id＋同一 CSR
    let csr_sha = sha256(&checked.csr_der);
    let host = tx.host_enroll(agent)?;
    let old_cert = tx.get_cert(&old.serial)?;
    if let Some(r) = tx.request_by_csr(agent, &csr_sha)? {
        match r.state {
            DbState::Issued => {
                let cert = match &r.issued_serial {
                    Some(s) => tx.get_cert(s)?,
                    None => None,
                };
                let latest = cert.as_ref().is_some_and(|c| {
                    c.revoked_at.is_none()
                        && host
                            .as_ref()
                            .and_then(|h| h.current_serial.as_deref())
                            .is_some_and(|s| s == c.serial)
                });
                if latest {
                    tx.set_last_grace_attempt(agent, &attempt("issued"))?;
                    tx.commit()?;
                    return Ok(GraceOutcome::Respond(Box::new((r, cert))));
                }
            }
            s if s.is_pending() => {
                tx.update_poll_secret(&r.request_id, &sha256(&checked.poll_secret))?;
                let r = tx.get_request(&r.request_id)?.unwrap_or(r);
                tx.commit()?;
                return Ok(GraceOutcome::Respond(Box::new((r, None))));
            }
            _ => {}
        }
    }
    // 10. 舊憑證已撤銷，或主機已除役
    if old_cert.as_ref().is_some_and(|c| c.revoked_at.is_some())
        || host
            .as_ref()
            .is_some_and(|h| h.lifecycle == "decommissioned")
    {
        return fail(
            tx,
            EnrollErrorCode::CertificateRevoked,
            "certificate has been revoked",
        );
    }
    // 11. 舊憑證必須是這台主機最新的憑證
    let latest = match host.as_ref().and_then(|h| h.current_serial.as_deref()) {
        Some(current) => current == old.serial,
        // 清冊沒有最新序號（手動簽發、未匯入）：只要沒有撤銷紀錄
        None => old_cert.is_none() || old_cert.as_ref().is_some_and(|c| c.revoked_at.is_none()),
    };
    if !latest {
        return fail(
            tx,
            EnrollErrorCode::RenewIdentityMismatch,
            "certificate is not the latest certificate of this agent",
        );
    }
    // 12. 舊憑證尚未過期（依 Kairos 的時間）
    if old.not_after_ms > now {
        return fail(
            tx,
            EnrollErrorCode::CertificateNotExpired,
            "certificate has not expired yet",
        );
    }
    // 13. 寬限期
    let grace_days = eff.policy.expired_renew_grace_days;
    let grace_ms = crate::testhooks::grace_secs()
        .filter(|_| grace_days > 0)
        .map_or(i64::from(grace_days) * 86_400_000, |s| s * 1000);
    let grace_end = old.not_after_ms + grace_ms;
    let overridden = host
        .as_ref()
        .and_then(|h| h.grace_override_until)
        .is_some_and(|u| u > now);
    if grace_days == 0 || (now > grace_end && !overridden) {
        if grace_days == 0 && eff.mode == kairos_proto::enroll::EnrollMode::RenewOnly {
            warn!(
                "警告 KRS-W2011：註冊端點為僅續簽，但過期寬限期政策為 0 天，過期續簽一律被拒（agent_id {agent}）"
            );
        }
        tx.set_last_grace_attempt(agent, &attempt("renew_grace_exceeded"))?;
        tx.commit()?;
        let mut e = EnrollError::new(
            EnrollErrorCode::RenewGraceExceeded,
            "certificate expired beyond the renewal grace period",
        );
        e.body.grace_ended_at = Some(format_rfc3339_ms(grace_end.min(now)));
        return Ok(GraceOutcome::Error(e, false));
    }
    // 14. CSR；新公鑰與舊公鑰相同（或先前用過）時拒絕
    let csr = match check_csr(&checked.csr_der, agent) {
        Ok(c) => c,
        Err(m) => {
            tx.set_last_grace_attempt(agent, &attempt("invalid_csr"))?;
            tx.commit()?;
            let mut e = EnrollError::new(EnrollErrorCode::InvalidCsr, m);
            e.body.field = Some("csr".into());
            return Ok(GraceOutcome::Error(e, false));
        }
    };
    if csr.spki_der == old.spki_der || tx.spki_used_by_agent(agent, &csr.spki_sha256)? {
        tx.set_last_grace_attempt(agent, &attempt("invalid_csr"))?;
        tx.commit()?;
        let mut e = EnrollError::new(
            EnrollErrorCode::InvalidCsr,
            "the new key must differ from previous keys of this agent_id",
        );
        e.body.field = Some("csr".into());
        return Ok(GraceOutcome::Error(e, false));
    }
    // 15. L8
    {
        let mut b = svc.limits.l8.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(w) = b.take(
            Instant::now(),
            Duration::from_secs(u64::from(svc.config.enroll.retry_after_max_secs)),
        ) {
            svc.stats.busy.fetch_add(1, Ordering::Relaxed);
            tx.commit()?;
            return Ok(GraceOutcome::Error(
                EnrollError::limited(EnrollErrorCode::EnrollBusy, RateScope::Global, w, source_ip),
                false,
            ));
        }
    }
    // 16. 政策要求人工核准 → 待核准（L9）；否則簽發
    let needs_approval = eff.policy.renew_requires_approval;
    if needs_approval && tx.pending_count()? >= i64::from(svc.config.enroll.max_pending) {
        tx.commit()?;
        let mut e = EnrollError::limited(
            EnrollErrorCode::PendingQueueFull,
            RateScope::Global,
            Duration::from_secs(3600),
            source_ip,
        );
        e.retry_after = Some(3600);
        e.body.retry_after_ms = Some(3_600_000);
        return Ok(GraceOutcome::Error(e, false));
    }
    let request_id = new_uuid()?;
    let row = RequestRow {
        request_id: request_id.clone(),
        kind: DbKind::GraceRenewal,
        agent_id: agent.clone(),
        request_code: request_code_from_spki(&csr.spki_der).compact(),
        state: if needs_approval {
            DbState::PendingApproval
        } else {
            DbState::Approved
        },
        csr_der: checked.csr_der.clone(),
        csr_sha256: csr.csr_sha256.to_vec(),
        spki_sha256: csr.spki_sha256.to_vec(),
        poll_secret_hash: Some(sha256(&checked.poll_secret).to_vec()),
        token_id: None,
        expired_serial: Some(old.serial.clone()),
        previous_serial: None,
        source_ip: source_ip.to_string(),
        hostname: req.hostname.clone(),
        display_name: None,
        tags: None,
        os: None,
        host_fingerprint: Some(req.host_fingerprint.clone()),
        pulse_version: Some(req.pulse_version.clone()),
        server_verification: None,
        flags: Vec::new(),
        cert_lifetime_days: None,
        created_at: now,
        expires_at: now + i64::from(svc.config.enroll.request_expiry_days) * 86_400_000,
        decided_at: (!needs_approval).then_some(now),
        decided_by: (!needs_approval).then(|| "system".to_string()),
        reject_reason_code: None,
        reject_reason: None,
        issued_serial: None,
        issue_error: None,
        delivered_at: None,
        withdrawn_at: None,
        withdraw_reason: None,
        withdraw_initiated_by: None,
        withdraw_ip: None,
        closed_at: None,
    };
    tx.insert_request(&row)?;
    tx.set_last_grace_attempt(
        agent,
        &attempt(if needs_approval {
            "pending_approval"
        } else {
            "issued"
        }),
    )?;
    tx.audit(now,
        &Audit {
            actor: &format!("host:{}", &agent[..8]),
            actor_ip: Some(source_ip),
            category: "enroll",
            action: "renewal.create",
            object: Some(&request_id),
            result: "ok",
            details: Some(
                json!({"agent_id": agent, "expired_serial": old.serial, "requires_approval": needs_approval}),
            ),
            ..Audit::default()
        },
    )?;
    tx.commit()?;
    Ok(GraceOutcome::Created(Box::new(row), !needs_approval))
}

// ---------------------------------------------------------------------------
// 撤回
// ---------------------------------------------------------------------------

fn not_pending(state: DbState) -> EnrollError {
    let mut e = EnrollError::new(
        EnrollErrorCode::RequestNotPending,
        "enrollment request is not pending",
    );
    e.body.state = Some(state.protocol().0);
    e
}

async fn withdraw_challenge(
    ctx: &Ctx<'_>,
    request_id: &str,
) -> Result<Response<ApiBody>, EnrollError> {
    let svc = ctx.svc;
    ctx.check_l10(request_id)?;
    let Some((r, _)) = load_request(svc, request_id).await? else {
        return Err(EnrollError::request_not_found());
    };
    match r.state {
        DbState::PendingApproval | DbState::Approved | DbState::Withdrawn | DbState::Issued => {}
        s => return Err(not_pending(s)),
    }
    let mut b = [0u8; kairos_proto::enroll::NONCE_BYTES];
    SystemRandom::new().fill(&mut b).map_err(|_| {
        EnrollError::new(EnrollErrorCode::Internal, "random number generator failed")
    })?;
    let nonce = b64url_encode(&b);
    let now = svc.now_ms();
    let expires = now + kairos_proto::enroll::WITHDRAW_NONCE_TTL_MS;
    svc.issue_nonce(&r.request_id, nonce.clone(), expires);
    let resp = WithdrawChallenge {
        server_time: String::new(),
        request_id: r.request_id.clone(),
        nonce,
        nonce_expires_at: format_rfc3339_ms(expires),
    };
    Ok(h::ok(&resp, now))
}

async fn withdraw(
    ctx: &Ctx<'_>,
    request_id: &str,
    body: &[u8],
) -> Result<Response<ApiBody>, EnrollError> {
    let svc = ctx.svc;
    ctx.check_l10(request_id)?;
    let req: WithdrawRequest = h::parse_json(body)?;
    let checked = validate_withdraw_request(request_id, &req).map_err(field_err)?;
    // nonce：任何一次驗證嘗試都作廢
    let now = svc.now_ms();
    match svc.take_nonce(&checked.request_id) {
        Some((n, _)) if n != req.nonce => {
            return Err(EnrollError::new(
                EnrollErrorCode::NonceInvalid,
                "nonce does not belong to this request or was already used",
            ));
        }
        None => {
            return Err(EnrollError::new(
                EnrollErrorCode::NonceInvalid,
                "nonce does not belong to this request or was already used",
            ));
        }
        Some((_, exp)) if exp <= now => {
            return Err(EnrollError::new(
                EnrollErrorCode::NonceExpired,
                "nonce has expired",
            ));
        }
        Some(_) => {}
    }
    let Some((r, _)) = load_request(svc, &checked.request_id).await? else {
        return Err(EnrollError::request_not_found());
    };
    // agent_id 與申請 CSR 的 SAN 相符，並以建立申請時保存的 CSR 公鑰驗證簽章
    let spki = check_csr(&r.csr_der, &r.agent_id).map(|c| c.spki_der).ok();
    let verified = r.agent_id == checked.agent_id
        && spki.is_some_and(|spki| {
            svc.endpoint_names().iter().any(|name| {
                withdraw_signing_input(&checked.request_id, &checked.agent_id, &req.nonce, name)
                    .is_ok_and(|input| {
                        verify_ecdsa_p256_sha256(&spki, &input, &req.signature).is_ok()
                    })
            })
        });
    if !verified {
        svc.record_invalid(
            ctx.src,
            InvalidClass::Renew,
            sha256(format!("{}|{}", checked.request_id, req.signature).as_bytes()),
        )
        .await;
        let id = checked.request_id.clone();
        let ip = ctx.observed.clone();
        let _ = svc
            .db
            .write(move |conn| {
                conn.audit(
                    now,
                    &Audit {
                        actor: "system",
                        actor_ip: Some(&ip),
                        category: "enroll",
                        action: "request.withdraw",
                        object: Some(&id),
                        result: "rejected",
                        code: Some("withdraw_signature_invalid"),
                        ..Audit::default()
                    },
                )?;
                Ok(())
            })
            .await;
        return Err(EnrollError::new(
            EnrollErrorCode::WithdrawSignatureInvalid,
            "withdraw signature is invalid",
        ));
    }
    let id = checked.request_id.clone();
    let ip = ctx.observed.clone();
    let reason = serde_json::to_value(req.reason)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let by = serde_json::to_value(req.initiated_by)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let outcome = svc
        .db
        .write(move |conn| {
            let tx = conn.begin(TxMode::Immediate)?;
            let Some(cur) = tx.get_request(&id)? else {
                return Ok(Err(EnrollError::request_not_found()));
            };
            let result = match cur.state {
                s if s.is_pending() => {
                    tx.withdraw(&id, &reason, &by, &ip, now)?;
                    Ok((tx.get_request(&id)?.unwrap_or(cur), false))
                }
                DbState::Withdrawn => Ok((cur, true)),
                DbState::Issued => {
                    // 撤回前剛被核准：不變更，加註並寫稽核（設計包 S05、#21 (e)）
                    tx.add_flag(&id, "removed_before_pickup")?;
                    Err(not_pending(DbState::Issued))
                }
                s => Err(not_pending(s)),
            };
            tx.audit(now,
                &Audit {
                    actor: "system",
                    actor_ip: Some(&ip),
                    category: "enroll",
                    action: "request.withdraw",
                    object: Some(&id),
                    result: if result.is_ok() { "ok" } else { "rejected" },
                    details: Some(json!({"reason": reason, "initiated_by": by, "state_before": cur_state_str(&result)})),
                    ..Audit::default()
                },
            )?;
            tx.commit()?;
            Ok(result)
        })
        .await
        .map_err(db_err)?;
    let (r, already) = outcome?;
    svc.notify_request(&r.request_id);
    if !already {
        info!(
            "註冊申請 {}（{}）已由申請者撤回（{}，來源 {}）",
            format_code(&r.request_code),
            r.hostname.escape_debug(),
            r.withdraw_initiated_by.as_deref().unwrap_or("-"),
            ctx.observed
        );
    }
    let resp = WithdrawResult {
        server_time: String::new(),
        request_id: r.request_id.clone(),
        request_code: RequestCode::parse(&r.request_code).ok_or_else(EnrollError::storage)?,
        state: kairos_proto::enroll::RequestState::Withdrawn,
        withdrawn_at: format_rfc3339_ms(r.withdrawn_at.unwrap_or(now)),
        already_withdrawn: already,
    };
    Ok(h::ok(&resp, svc.now_ms()))
}

fn cur_state_str(r: &Result<(RequestRow, bool), EnrollError>) -> &'static str {
    match r {
        Ok((_, true)) => "withdrawn",
        Ok((_, false)) => "pending_approval",
        Err(_) => "not_pending",
    }
}
