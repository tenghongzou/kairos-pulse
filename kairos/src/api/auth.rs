//! 驗證（暫行：API token，`kairos/API.md` 第 4 節）。
//!
//! - [`Authenticator::authenticate`] 從 `Authorization: Bearer` 取出 token，以 token_id 查詢資料庫，
//!   以常數時間比較秘密的 SHA-256；token_id 不存在時仍比較一次（回應時間不洩漏 token_id 是否存在）。
//! - 同一來源（IPv6 以 /64 合併）驗證失敗時扣額度（[`FailureLimiter`]），用完時該來源的請求一律 429。
//! - 得到 [`Principal`]：之後的工作階段（D25）與角色（`ia.md` §4）加在這裡，端點以
//!   [`Principal::require`] 檢查權限，不需修改。

use chrono::{DateTime, Utc};
use hyper::header::{AUTHORIZATION, HeaderMap};
use log::warn;
use serde::Serialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

use super::db::{ReadPool, query_failed};
use super::http::{ApiError, CODE_RATE_LIMITED, ts_ms};
use super::tokens::{self, SCOPE_READ, StoredToken, hash_secret, parse_token};
use crate::per_ip::source_key;

/// 驗證失敗的額度：可連續幾次
pub const FAILURE_BURST: f64 = 10.0;
/// 驗證失敗的額度：每秒恢復幾次（每 6 秒 1 次）
pub const FAILURE_REFILL_PER_SEC: f64 = 1.0 / 6.0;
/// 追蹤的來源數上限
pub const MAX_TRACKED_SOURCES: usize = 10_000;

/// 權限（之後的角色對應到一組權限；`ia.md` §4.2、API.md 第 11.1 節）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// 所有讀取端點（主機、趨勢、總覽、系統健康、事件）
    Read,
    /// 建立、停用註冊 token（`operate` 只能建立需核准的 token）
    EnrollTokenWrite,
    /// 建立自動核准的 token；自動核准 token 的批次部署模式
    EnrollTokenAuto,
    /// 核准、拒絕申請
    EnrollDecide,
    /// 核准後會超過授權節點數（本版沒有授權機制，保留給之後的版本）
    #[allow(dead_code)]
    EnrollLicenseOverflow,
    /// 切換註冊端點模式、自動暫停
    EnrollModeWrite,
    /// 撤銷憑證、立即重新產生 CRL
    CertRevoke,
    /// 憑證政策
    CertPolicyWrite,
    /// 允許再次自動續簽
    GraceOverride,
    /// 離線模式的 CSR 佇列匯出與匯入
    PkiQueue,
    /// 簽發 CA 的建立、安裝、啟用、退役（本版只有 CLI，API 之後再加）
    #[allow(dead_code)]
    PkiCaWrite,
    /// 告警事件的查詢（S06、S07）
    AlertsRead,
    /// 確認、指派、暫停通知、解決、註解（S06、S07）
    AlertsWrite,
    /// 規則的查詢（S08）
    AlertRulesRead,
    /// 建立與編輯自訂規則、啟停規則、調整系統預設規則的通知（S08、S09：維運者）
    AlertRulesWrite,
    /// 調整系統預設規則的條件（S09：只有管理員）
    AlertRulesAdmin,
    /// 靜音與維護時段的查詢（S10）
    SilencesRead,
    /// 建立、修改、提前結束靜音與維護時段（S10：維運者）
    SilencesWrite,
    /// 通知管道的清單與狀態（S11：維運者只能看清單與寄送測試，稽核員與唯讀看不到）
    ChannelsRead,
    /// 寄送測試通知（S11）
    ChannelsTest,
    /// 新增、修改、刪除通知管道（S11：管理員）
    ChannelsWrite,
}

impl Permission {
    /// 需要的範圍（read＝0、operate＝1、admin＝2）
    pub fn required_scope(self) -> &'static str {
        match self {
            Permission::Read
            | Permission::AlertsRead
            | Permission::AlertRulesRead
            | Permission::SilencesRead => SCOPE_READ,
            Permission::EnrollTokenWrite
            | Permission::EnrollDecide
            | Permission::CertRevoke
            | Permission::PkiQueue
            | Permission::AlertsWrite
            | Permission::AlertRulesWrite
            | Permission::SilencesWrite
            | Permission::ChannelsRead
            | Permission::ChannelsTest => super::tokens::SCOPE_OPERATE,
            _ => super::tokens::SCOPE_ADMIN,
        }
    }

    /// 權限的名稱（`alerts:read` 形式；API 文件與之後的角色對應使用）
    pub fn name(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::EnrollTokenWrite => "enroll-tokens:write",
            Permission::EnrollTokenAuto => "enroll-tokens:auto",
            Permission::EnrollDecide => "enroll:decide",
            Permission::EnrollLicenseOverflow => "enroll:license-overflow",
            Permission::EnrollModeWrite => "enroll:mode",
            Permission::CertRevoke => "certificates:revoke",
            Permission::CertPolicyWrite => "certificates:policy",
            Permission::GraceOverride => "certificates:grace-override",
            Permission::PkiQueue => "pki:queue",
            Permission::PkiCaWrite => "pki:ca",
            Permission::AlertsRead => "alerts:read",
            Permission::AlertsWrite => "alerts:write",
            Permission::AlertRulesRead => "alert-rules:read",
            Permission::AlertRulesWrite => "alert-rules:write",
            Permission::AlertRulesAdmin => "alert-rules:admin",
            Permission::SilencesRead => "silences:read",
            Permission::SilencesWrite => "silences:write",
            Permission::ChannelsRead => "channels:read",
            Permission::ChannelsTest => "channels:test",
            Permission::ChannelsWrite => "channels:write",
        }
    }
}

/// 通過驗證的身分
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Principal {
    ApiToken {
        token_id: String,
        name: String,
        scopes: Vec<String>,
        expires_at: Option<String>,
    },
}

impl Principal {
    /// 檢查權限；沒有時回 403 `forbidden`
    pub fn require(&self, permission: Permission) -> Result<(), ApiError> {
        let need = super::tokens::scope_rank(permission.required_scope()).unwrap_or(u8::MAX);
        let allowed = match self {
            Principal::ApiToken { scopes, .. } => scopes
                .iter()
                .filter_map(|s| super::tokens::scope_rank(s))
                .any(|rank| rank >= need),
        };
        if allowed {
            Ok(())
        } else {
            Err(ApiError::forbidden().with_details(serde_json::json!({
                "required": permission.required_scope(),
                "permission": permission.name(),
            })))
        }
    }

    /// 日誌中的身分（不含秘密）
    pub fn label(&self) -> String {
        match self {
            Principal::ApiToken { token_id, .. } => format!("token {token_id}"),
        }
    }

    pub fn token_id(&self) -> &str {
        match self {
            Principal::ApiToken { token_id, .. } => token_id,
        }
    }
}

/// 一個來源的失敗額度（token bucket）
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
    /// 已記過「開始限制」的日誌
    logged: bool,
}

/// 驗證失敗的速率限制
#[derive(Debug, Default)]
pub struct FailureLimiter {
    sources: Mutex<HashMap<IpAddr, Bucket>>,
}

impl FailureLimiter {
    fn refill(b: &mut Bucket, now: Instant) {
        let elapsed = now.saturating_duration_since(b.updated).as_secs_f64();
        b.tokens = (b.tokens + elapsed * FAILURE_REFILL_PER_SEC).min(FAILURE_BURST);
        b.updated = now;
    }

    /// 請求開始時檢查：額度用完時回傳恢復 1 次所需的秒數
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        let key = source_key(ip);
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        let Some(b) = sources.get_mut(&key) else {
            return Ok(());
        };
        Self::refill(b, now);
        if b.tokens >= 1.0 {
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / FAILURE_REFILL_PER_SEC).ceil().max(1.0) as u64)
        }
    }

    /// 驗證失敗時扣一次額度；回傳是否剛開始被限制（第一次用完，供節流的日誌）
    pub fn fail(&self, ip: IpAddr, now: Instant) -> bool {
        let key = source_key(ip);
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        if !sources.contains_key(&key) && sources.len() >= MAX_TRACKED_SOURCES {
            // 清掉最久沒有失敗的來源
            if let Some(oldest) = sources
                .iter()
                .min_by_key(|(_, b)| b.updated)
                .map(|(k, _)| *k)
            {
                sources.remove(&oldest);
            }
        }
        let b = sources.entry(key).or_insert(Bucket {
            tokens: FAILURE_BURST,
            updated: now,
            logged: false,
        });
        Self::refill(b, now);
        b.tokens = (b.tokens - 1.0).max(0.0);
        if b.tokens < 1.0 && !b.logged {
            b.logged = true;
            true
        } else {
            if b.tokens >= FAILURE_BURST - 1.0 {
                b.logged = false;
            }
            false
        }
    }

    /// 清掉已恢復滿額度的來源
    pub fn prune(&self, now: Instant) {
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        sources.retain(|_, b| {
            Self::refill(b, now);
            b.tokens < FAILURE_BURST
        });
    }
}

/// 最近一次成功使用（記憶體中；背景工作每 60 秒寫回資料庫）
#[derive(Debug, Default)]
pub struct LastUsed {
    entries: Mutex<HashMap<String, (i64, String)>>,
}

impl LastUsed {
    pub fn note(&self, token_id: &str, at_ms: i64, ip: IpAddr) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(token_id.to_string(), (at_ms, ip.to_string()));
    }

    /// 取出所有待寫回的紀錄
    pub fn take(&self) -> Vec<(String, i64, String)> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.drain().map(|(id, (at, ip))| (id, at, ip)).collect()
    }

    /// 寫回失敗時放回（不覆蓋較新的）
    pub fn restore(&self, items: Vec<(String, i64, String)>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for (id, at, ip) in items {
            entries.entry(id).or_insert((at, ip));
        }
    }
}

/// token 的驗證
pub struct Authenticator {
    pool: Arc<ReadPool>,
    pub failures: FailureLimiter,
    pub last_used: LastUsed,
}

/// 找不到 token_id 時用來比較的雜湊（讓兩種情況的計算量相同）
const DUMMY_HASH: [u8; 32] = [0x5a; 32];

impl Authenticator {
    pub fn new(pool: Arc<ReadPool>) -> Self {
        Self {
            pool,
            failures: FailureLimiter::default(),
            last_used: LastUsed::default(),
        }
    }

    /// 驗證請求；成功時記下最後使用的時間與來源
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        peer: IpAddr,
        now: DateTime<Utc>,
    ) -> Result<Principal, ApiError> {
        if let Err(retry) = self.failures.check(peer, Instant::now()) {
            return Err(ApiError::rate_limited(retry));
        }
        let token = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                let (scheme, rest) = v.trim().split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
            })
            .map(str::to_string);
        let Some(token) = token else {
            return Err(self.failed(peer, "沒有 Authorization: Bearer"));
        };
        let Some(parsed) = parse_token(&token) else {
            return Err(self.failed(peer, "token 格式不正確"));
        };
        let token_id = parsed.token_id.to_string();
        let presented = hash_secret(parsed.secret);
        let id = token_id.clone();
        let stored = self
            .pool
            .read("API token", move |conn| {
                tokens::lookup(conn, &id).map_err(query_failed("API token"))
            })
            .await?;
        let valid = verify(stored.as_ref(), &presented, now.timestamp_millis());
        let Some(stored) = stored.filter(|_| valid) else {
            return Err(self.failed(peer, "token 不存在、已撤銷、已過期或不正確"));
        };
        self.last_used.note(&token_id, now.timestamp_millis(), peer);
        Ok(Principal::ApiToken {
            token_id,
            name: stored.name,
            scopes: stored
                .scope
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            expires_at: stored.expires_at.map(ts_ms),
        })
    }

    /// token 是否仍有效（SSE 每次心跳時檢查：撤銷或過期後關閉串流）
    pub async fn still_valid(&self, token_id: &str, now: DateTime<Utc>) -> bool {
        let id = token_id.to_string();
        let stored = self
            .pool
            .read("API token", move |conn| {
                tokens::lookup(conn, &id).map_err(query_failed("API token"))
            })
            .await;
        match stored {
            Ok(Some(s)) => {
                s.revoked_at.is_none() && s.expires_at.is_none_or(|e| e > now.timestamp_millis())
            }
            Ok(None) => false,
            // 資料庫暫時無法查詢時不中斷串流
            Err(_) => true,
        }
    }

    fn failed(&self, peer: IpAddr, why: &str) -> ApiError {
        if self.failures.fail(peer, Instant::now()) {
            warn!(
                "警告 {}：主控台 API：來自 {} 的請求驗證失敗太多次（{}），之後每 {} 秒只接受 1 次嘗試",
                CODE_RATE_LIMITED,
                source_key(peer),
                why,
                Duration::from_secs_f64(1.0 / FAILURE_REFILL_PER_SEC).as_secs()
            );
        }
        ApiError::unauthorized()
    }
}

/// 比較雜湊（常數時間）並檢查撤銷與到期
fn verify(stored: Option<&StoredToken>, presented: &[u8; 32], now_ms: i64) -> bool {
    let expected: &[u8] = stored.map_or(&DUMMY_HASH[..], |s| s.secret_hash.as_slice());
    let same: bool = if expected.len() == presented.len() {
        expected.ct_eq(presented).into()
    } else {
        false
    };
    let Some(s) = stored else {
        return false;
    };
    same && s.revoked_at.is_none() && s.expires_at.is_none_or(|e| e > now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_limiter_blocks_after_the_burst_and_recovers() {
        let l = FailureLimiter::default();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        assert!(l.check(ip, t0).is_ok());
        let mut started = 0;
        for _ in 0..FAILURE_BURST as usize {
            if l.fail(ip, t0) {
                started += 1;
            }
        }
        // 第 10 次失敗後額度用完：只記一次「開始限制」
        assert_eq!(started, 1);
        assert_eq!(l.check(ip, t0), Err(6));
        // 其他來源不受影響；同一個 IPv6 /64 視為同一個來源
        assert!(l.check("192.0.2.2".parse().unwrap(), t0).is_ok());
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::2".parse().unwrap();
        for _ in 0..10 {
            l.fail(a, t0);
        }
        assert!(l.check(b, t0).is_err());
        // 約 6 秒後恢復 1 次
        assert!(l.check(ip, t0 + Duration::from_secs(7)).is_ok());
        // 一分鐘後恢復滿額度，被清除
        l.prune(t0 + Duration::from_secs(120));
        assert!(l.sources.lock().unwrap().is_empty());
    }

    #[test]
    fn verification_checks_hash_revocation_and_expiry() {
        let secret_hash = hash_secret("s3cret").to_vec();
        let stored = StoredToken {
            secret_hash,
            name: "t".into(),
            scope: "read".into(),
            expires_at: Some(10_000),
            revoked_at: None,
        };
        let good = hash_secret("s3cret");
        let bad = hash_secret("other");
        assert!(verify(Some(&stored), &good, 9_999));
        assert!(!verify(Some(&stored), &bad, 9_999));
        assert!(!verify(Some(&stored), &good, 10_000));
        assert!(!verify(None, &good, 0));
        let revoked = StoredToken {
            revoked_at: Some(1),
            ..stored.clone()
        };
        assert!(!verify(Some(&revoked), &good, 0));
        let forever = StoredToken {
            expires_at: None,
            ..stored
        };
        assert!(verify(Some(&forever), &good, i64::MAX));
    }

    #[test]
    fn principal_permissions() {
        let p = Principal::ApiToken {
            token_id: "0123456789abcdef".into(),
            name: "t".into(),
            scopes: vec!["read".into()],
            expires_at: None,
        };
        assert!(p.require(Permission::Read).is_ok());
        let none = Principal::ApiToken {
            token_id: "0123456789abcdef".into(),
            name: "t".into(),
            scopes: vec![],
            expires_at: None,
        };
        let denied = none.require(Permission::Read).unwrap_err();
        assert_eq!(denied.status, hyper::StatusCode::FORBIDDEN);
        assert_eq!(denied.code, "forbidden");
        assert_eq!(denied.product_code, Some("KRS-E6104"));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["type"], "api_token");
        assert_eq!(v["scopes"][0], "read");
        assert_eq!(p.label(), "token 0123456789abcdef");
    }
}
