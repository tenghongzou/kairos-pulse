//! Pulse 端的註冊狀態機（設計包 `enrollment-ux.md` §4.1、ENROLL.md 第 6.3、6.4、7.3 節）：
//! 把 8444 的回應與錯誤轉成「下一個狀態、要等多久、對應的代碼」。純函式，不做 I/O。

use std::time::Duration;

use kairos_proto::enroll::{EnrollErrorCode as E, EnrollResult, RequestKind, RequestState};

use super::client::ApiError;
use super::schedule::{self, Exp};
use super::store::{grace_state, state};
use crate::codes;

/// 請求的種類（同一個錯誤碼在不同請求的處置不同）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 新註冊（`POST /v1/enroll/requests`）
    Submit,
    /// 輪詢（新註冊或過期續簽的申請）
    Poll,
    /// 過期續簽（`POST /v1/enroll/renewals`）
    Grace,
}

/// 錯誤的處置
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// 等待後重送同一個請求（同一個 CSR、同一個 `poll_secret`，新的 `sent_at`）
    Retry {
        wait: Duration,
        /// 呈現的代碼（`PLS-E2202`、`PLS-E3309`、`PLS-E2003` 等）
        code: &'static str,
        /// 這次等待的狀態（`submitting`、`clock_rejected`、`grace_*`…）
        state: &'static str,
        /// 依伺服器的 `Retry-After`（429／503）
        rate_limited: bool,
    },
    /// Kairos 因時鐘偏差暫不受理（`clock_skew`）：每 5 分鐘重送，偵測到時間調整時立即
    ClockRejected {
        wait: Duration,
        /// 本機減 Kairos（負值＝本機較慢；`error.offset_ms` 的定義相同）
        offset_ms: Option<i64>,
        server_time: String,
    },
    /// 立即以新的 `sent_at` 重新簽章重送一次（過期續簽的 `replayed_request`）
    ResignOnce,
    /// 不再自動重試
    Stop {
        state: &'static str,
        code: &'static str,
    },
}

/// 亂數來源（`[0, 1)`）
pub type Rand<'a> = &'a mut dyn FnMut() -> f64;

/// 新註冊與輪詢的錯誤處置
pub fn on_error(
    err: &ApiError,
    phase: Phase,
    net: &mut Exp,
    clock_retry: Duration,
    rand: Rand<'_>,
) -> Decision {
    let waiting = match phase {
        Phase::Submit => state::SUBMITTING,
        Phase::Poll => state::PENDING_APPROVAL,
        Phase::Grace => grace_state::RENEWING,
    };
    match err {
        ApiError::Network { code, refused, .. } => Decision::Retry {
            wait: net.next(rand()),
            // 8444 拒絕連線多半是註冊端點為「關閉」（PLS-E2007）
            code: if *refused {
                codes::E_ENROLL_UNREACHABLE
            } else {
                code
            },
            state: waiting,
            rate_limited: false,
        },
        ApiError::Trust { code, .. } => Decision::Stop {
            state: state::FAILED,
            code,
        },
        ApiError::Unsupported { .. } => match phase {
            // 輪詢時 404 不是本規格的本文：申請已不存在或端點消失，與 request_not_found 同樣視為逾期
            Phase::Poll => Decision::Stop {
                state: state::REQUEST_EXPIRED,
                code: codes::E_REQUEST_EXPIRED,
            },
            _ => Decision::Stop {
                state: state::SERVER_UNSUPPORTED,
                code: codes::E_ENROLL_UNSUPPORTED,
            },
        },
        ApiError::Unexpected { .. } => Decision::Retry {
            wait: net.next(rand()),
            code: codes::W_SERVER_UNEXPECTED,
            state: waiting,
            rate_limited: false,
        },
        ApiError::Server(e) => {
            let hint = e.wait_hint();
            let mut retry_after = |code| {
                let (u1, u2) = (rand(), rand());
                Decision::Retry {
                    wait: schedule::retry_after_wait(
                        hint.unwrap_or(Duration::from_secs(60)),
                        u1,
                        u2,
                    ),
                    code,
                    state: waiting,
                    rate_limited: true,
                }
            };
            match e.code {
                E::ClockSkew => {
                    let u = rand();
                    Decision::ClockRejected {
                        wait: schedule::jitter(clock_retry, u),
                        offset_ms: e.body.offset_ms,
                        server_time: e.server_time.clone(),
                    }
                }
                E::RateLimited | E::EnrollBusy => retry_after(codes::E_ENROLL_RATE_LIMITED),
                E::PendingQueueFull => retry_after(codes::E_PENDING_QUEUE_FULL),
                // 服務等封鎖結束後重試 1 次（呼叫端記錄次數）；互動式 CLI 直接結束
                E::SourceBlocked => retry_after(codes::E_SOURCE_BLOCKED),
                E::StorageUnavailable | E::ShuttingDown | E::Internal => match hint {
                    Some(_) => match retry_after(codes::W_SERVER_UNEXPECTED) {
                        Decision::Retry { wait, .. } => Decision::Retry {
                            wait,
                            code: codes::W_SERVER_UNEXPECTED,
                            state: waiting,
                            rate_limited: true,
                        },
                        d => d,
                    },
                    None => {
                        let u = rand();
                        Decision::Retry {
                            wait: net.next(u),
                            code: codes::W_SERVER_UNEXPECTED,
                            state: waiting,
                            rate_limited: false,
                        }
                    }
                },
                E::TokenInvalid | E::TokenExpired | E::TokenExhausted => Decision::Stop {
                    state: state::TOKEN_ERROR,
                    code: e.code.pulse_code().unwrap_or(codes::E_TOKEN_INVALID),
                },
                E::EnrollNotAccepted => Decision::Stop {
                    state: state::NOT_ACCEPTED,
                    code: codes::E_ENROLL_NOT_ACCEPTED,
                },
                E::RequestNotFound if phase == Phase::Poll => Decision::Stop {
                    state: state::REQUEST_EXPIRED,
                    code: codes::E_REQUEST_EXPIRED,
                },
                E::InvalidCsr => Decision::Stop {
                    state: state::FAILED,
                    code: codes::E_REQUEST_INVALID,
                },
                E::LicenseExhausted => Decision::Stop {
                    state: state::FAILED,
                    code: codes::E_LICENSE_EXHAUSTED,
                },
                E::NotFound | E::MethodNotAllowed => Decision::Stop {
                    state: state::SERVER_UNSUPPORTED,
                    code: codes::E_ENROLL_UNSUPPORTED,
                },
                // bad_request、request_conflict、payload_too_large 等：重送同一個請求也不會成功
                _ => Decision::Stop {
                    state: state::FAILED,
                    code: codes::W_SERVER_UNEXPECTED,
                },
            }
        }
    }
}

/// 過期續簽的錯誤處置（ENROLL.md 第 7.3 節、設計包 §8 的重試排程）。`backoff` 為 2 → 60 分鐘的退避
pub fn on_grace_error(
    err: &ApiError,
    backoff: &mut Exp,
    clock_retry: Duration,
    resigned: bool,
    rand: Rand<'_>,
) -> Decision {
    match err {
        ApiError::Network { code, refused, .. } => Decision::Retry {
            wait: backoff.next(rand()),
            code: if *refused {
                codes::E_ENROLL_UNREACHABLE
            } else {
                code
            },
            state: grace_state::BLOCKED,
            rate_limited: false,
        },
        ApiError::Server(e) => match e.code {
            E::ClockSkew => Decision::ClockRejected {
                wait: schedule::jitter(clock_retry, rand()),
                offset_ms: e.body.offset_ms,
                server_time: e.server_time.clone(),
            },
            E::ReplayedRequest if !resigned => Decision::ResignOnce,
            E::ReplayedRequest => Decision::Retry {
                wait: backoff.next(rand()),
                code: codes::W_SERVER_UNEXPECTED,
                state: grace_state::RENEWING,
                rate_limited: false,
            },
            E::RateLimited | E::EnrollBusy | E::SourceBlocked | E::PendingQueueFull => {
                let code = match e.code {
                    E::SourceBlocked => codes::E_SOURCE_BLOCKED,
                    E::PendingQueueFull => codes::E_PENDING_QUEUE_FULL,
                    _ => codes::E_ENROLL_RATE_LIMITED,
                };
                Decision::Retry {
                    wait: schedule::grace_rate_limited_wait(
                        e.wait_hint().unwrap_or(Duration::from_secs(60)),
                        backoff,
                        rand(),
                        rand(),
                    ),
                    code,
                    state: grace_state::RENEWING,
                    rate_limited: true,
                }
            }
            E::RenewGraceExceeded => Decision::Retry {
                wait: schedule::jitter(schedule::GRACE_EXCEEDED_RETRY, rand()),
                code: codes::E_GRACE_EXCEEDED,
                state: grace_state::EXCEEDED,
                rate_limited: false,
            },
            E::RenewIdentityMismatch => Decision::Stop {
                state: grace_state::REJECTED,
                code: codes::E_GRACE_IDENTITY,
            },
            E::CertificateRevoked => Decision::Stop {
                state: grace_state::REVOKED,
                code: codes::E_CLIENT_CERT_REJECTED,
            },
            // 依 Kairos 的時間仍有效：回到 8443（呼叫端處理）
            E::CertificateNotExpired => Decision::Stop {
                state: state::REGISTERED,
                code: codes::E_CLIENT_CERT_EXPIRED,
            },
            E::InvalidCsr => Decision::Stop {
                state: grace_state::REJECTED,
                code: codes::E_REQUEST_INVALID,
            },
            _ => Decision::Retry {
                wait: backoff.next(rand()),
                code: codes::W_SERVER_UNEXPECTED,
                state: grace_state::RENEWING,
                rate_limited: false,
            },
        },
        ApiError::Unsupported { .. } => Decision::Retry {
            wait: backoff.next(rand()),
            code: codes::E_ENROLL_UNSUPPORTED,
            state: grace_state::BLOCKED,
            rate_limited: false,
        },
        ApiError::Trust { code, .. } => Decision::Retry {
            wait: backoff.next(rand()),
            code,
            state: grace_state::BLOCKED,
            rate_limited: false,
        },
        ApiError::Unexpected { .. } => Decision::Retry {
            wait: backoff.next(rand()),
            code: codes::W_SERVER_UNEXPECTED,
            state: grace_state::RENEWING,
            rate_limited: false,
        },
    }
}

/// 申請的 `state` 對應的 Pulse 端狀態與代碼（ENROLL.md 第 12 節的表）
pub fn result_state(result: &EnrollResult) -> (&'static str, Option<&'static str>) {
    let grace = result.kind == RequestKind::GraceRenewal;
    match result.state {
        RequestState::PendingApproval if grace => {
            (grace_state::PENDING, Some(codes::I_PENDING_APPROVAL))
        }
        RequestState::PendingApproval => (state::PENDING_APPROVAL, Some(codes::I_PENDING_APPROVAL)),
        RequestState::Issued => (state::ISSUED, Some(codes::I_ISSUED)),
        RequestState::Rejected if grace => (grace_state::REJECTED, Some(codes::E_REQUEST_REJECTED)),
        RequestState::Rejected => (state::REJECTED, Some(codes::E_REQUEST_REJECTED)),
        // 撤回（本機已清除）、被新申請取代：都視同逾期，需要 pulse enroll --force
        RequestState::RequestExpired | RequestState::Replaced | RequestState::Withdrawn => {
            (state::REQUEST_EXPIRED, Some(codes::E_REQUEST_EXPIRED))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enroll::client::ServerError;
    use kairos_proto::enroll::{ErrorBody, RequestCode};

    fn server(code: E, retry_after: Option<u64>) -> ApiError {
        let mut body = ErrorBody::new(code, "x");
        body.retry_after_ms = retry_after.map(|s| s * 1000);
        if code == E::ClockSkew {
            body.offset_ms = Some(-462_120);
        }
        ApiError::Server(ServerError {
            status: code.http_status(),
            code,
            body,
            retry_after: retry_after.map(Duration::from_secs),
            server_time: "2026-10-11T02:20:47.120Z".into(),
        })
    }

    fn decide(err: &ApiError, phase: Phase) -> Decision {
        let mut net = Exp::new(schedule::NET_BACKOFF_INITIAL, schedule::NET_BACKOFF_MAX);
        let mut r = || 0.5;
        on_error(err, phase, &mut net, schedule::CLOCK_RETRY, &mut r)
    }

    #[test]
    fn submit_errors_follow_the_spec_table() {
        // 429／503：依 Retry-After 排隊，不早於伺服器指定的時間
        match decide(&server(E::RateLimited, Some(42)), Phase::Submit) {
            Decision::Retry {
                wait,
                code,
                state,
                rate_limited,
            } => {
                assert!(wait >= Duration::from_secs(42));
                assert_eq!(
                    (code, state, rate_limited),
                    (codes::E_ENROLL_RATE_LIMITED, state::SUBMITTING, true)
                );
            }
            d => panic!("{d:?}"),
        }
        assert!(matches!(
            decide(&server(E::EnrollBusy, Some(5)), Phase::Submit),
            Decision::Retry {
                code: codes::E_ENROLL_RATE_LIMITED,
                ..
            }
        ));
        assert!(matches!(
            decide(&server(E::PendingQueueFull, Some(3600)), Phase::Submit),
            Decision::Retry { code: codes::E_PENDING_QUEUE_FULL, wait, .. } if wait >= Duration::from_secs(3600)
        ));
        // token_* 與 enroll_not_accepted 停止
        for (code, pls) in [
            (E::TokenInvalid, "PLS-E3002"),
            (E::TokenExpired, "PLS-E3003"),
            (E::TokenExhausted, "PLS-E3004"),
        ] {
            assert_eq!(
                decide(&server(code, None), Phase::Submit),
                Decision::Stop {
                    state: state::TOKEN_ERROR,
                    code: pls
                }
            );
        }
        assert_eq!(
            decide(&server(E::EnrollNotAccepted, None), Phase::Submit),
            Decision::Stop {
                state: state::NOT_ACCEPTED,
                code: "PLS-E2008"
            }
        );
        assert_eq!(
            decide(&server(E::LicenseExhausted, None), Phase::Submit),
            Decision::Stop {
                state: state::FAILED,
                code: "PLS-E7101"
            }
        );
        assert_eq!(
            decide(&server(E::InvalidCsr, None), Phase::Submit),
            Decision::Stop {
                state: state::FAILED,
                code: "PLS-E3304"
            }
        );
        // clock_skew：每 5 分鐘 ±20%，帶 offset 與 server_time
        match decide(&server(E::ClockSkew, None), Phase::Submit) {
            Decision::ClockRejected {
                wait,
                offset_ms,
                server_time,
            } => {
                assert_eq!(wait, Duration::from_secs(300));
                assert_eq!(offset_ms, Some(-462_120));
                assert_eq!(server_time, "2026-10-11T02:20:47.120Z");
            }
            d => panic!("{d:?}"),
        }
        // Kairos 不支援
        assert_eq!(
            decide(&ApiError::Unsupported { status: 404 }, Phase::Submit),
            Decision::Stop {
                state: state::SERVER_UNSUPPORTED,
                code: "PLS-E2106"
            }
        );
        // 8444 拒絕連線：PLS-E2007，網路退避 2 秒起
        match decide(
            &ApiError::Network {
                code: codes::E_CONN_REFUSED,
                detail: String::new(),
                refused: true,
            },
            Phase::Submit,
        ) {
            Decision::Retry { wait, code, .. } => {
                assert_eq!(wait, Duration::from_secs(2));
                assert_eq!(code, "PLS-E2007");
            }
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn poll_errors() {
        assert_eq!(
            decide(&server(E::RequestNotFound, None), Phase::Poll),
            Decision::Stop {
                state: state::REQUEST_EXPIRED,
                code: "PLS-E3303"
            }
        );
        assert!(matches!(
            decide(&server(E::RateLimited, Some(10)), Phase::Poll),
            Decision::Retry {
                state: state::PENDING_APPROVAL,
                rate_limited: true,
                ..
            }
        ));
    }

    #[test]
    fn grace_errors_follow_section_7_3() {
        let mut b = Exp::new(schedule::GRACE_BACKOFF_INITIAL, schedule::GRACE_BACKOFF_MAX);
        let mut r = || 0.5;
        let mut d = |e: &ApiError, resigned| {
            on_grace_error(e, &mut b, schedule::CLOCK_RETRY, resigned, &mut r)
        };
        assert!(matches!(
            d(&server(E::ClockSkew, None), false),
            Decision::ClockRejected { .. }
        ));
        assert_eq!(
            d(&server(E::ReplayedRequest, None), false),
            Decision::ResignOnce
        );
        assert!(matches!(
            d(&server(E::ReplayedRequest, None), true),
            Decision::Retry {
                code: "PLS-W2107",
                state: grace_state::RENEWING,
                ..
            }
        ));
        assert!(matches!(
            d(&server(E::RenewGraceExceeded, None), false),
            Decision::Retry { code: "PLS-E1113", state: grace_state::EXCEEDED, wait, .. } if wait == Duration::from_secs(6 * 3600)
        ));
        assert_eq!(
            d(&server(E::RenewIdentityMismatch, None), false),
            Decision::Stop {
                state: grace_state::REJECTED,
                code: "PLS-E1114"
            }
        );
        assert_eq!(
            d(&server(E::CertificateRevoked, None), false),
            Decision::Stop {
                state: grace_state::REVOKED,
                code: "PLS-E1105"
            }
        );
        assert_eq!(
            d(&server(E::CertificateNotExpired, None), false),
            Decision::Stop {
                state: state::REGISTERED,
                code: "PLS-E1108"
            }
        );
        assert!(matches!(
            d(
                &ApiError::Network {
                    code: codes::E_CONN_REFUSED,
                    detail: String::new(),
                    refused: true
                },
                false
            ),
            Decision::Retry {
                code: "PLS-E2007",
                state: grace_state::BLOCKED,
                ..
            }
        ));
    }

    #[test]
    fn grace_rate_limit_does_not_reset_or_advance_backoff() {
        let mut b = Exp::new(schedule::GRACE_BACKOFF_INITIAL, schedule::GRACE_BACKOFF_MAX);
        let mut r = || 0.5;
        for _ in 0..3 {
            b.next(0.5);
        }
        let before = b;
        match on_grace_error(
            &server(E::RateLimited, Some(10)),
            &mut b,
            schedule::CLOCK_RETRY,
            false,
            &mut r,
        ) {
            Decision::Retry {
                wait,
                rate_limited: true,
                ..
            } => assert_eq!(wait, Duration::from_secs(960)),
            d => panic!("{d:?}"),
        }
        assert_eq!(b, before);
    }

    #[test]
    fn result_states_map_to_pulse_states() {
        let mut r = EnrollResult {
            server_time: String::new(),
            request_id: "0d5f7b2e-3c41-4a8e-9b6f-2e7c1d9a4b30".into(),
            request_code: RequestCode::parse("1KGJ-N9XN").unwrap(),
            kind: RequestKind::Enroll,
            state: RequestState::PendingApproval,
            agent_id: String::new(),
            expires_at: None,
            poll_after_ms: None,
            approval_detail: None,
            certificate: None,
            policy: None,
            decided_at: None,
            decided_by: None,
            reject_reason: None,
            observed_ip: None,
        };
        assert_eq!(
            result_state(&r),
            (state::PENDING_APPROVAL, Some("PLS-I3307"))
        );
        r.kind = RequestKind::GraceRenewal;
        assert_eq!(result_state(&r).0, grace_state::PENDING);
        r.kind = RequestKind::Enroll;
        for (s, expected) in [
            (RequestState::Issued, state::ISSUED),
            (RequestState::Rejected, state::REJECTED),
            (RequestState::RequestExpired, state::REQUEST_EXPIRED),
            (RequestState::Replaced, state::REQUEST_EXPIRED),
            (RequestState::Withdrawn, state::REQUEST_EXPIRED),
        ] {
            r.state = s;
            assert_eq!(result_state(&r).0, expected);
        }
    }
}
