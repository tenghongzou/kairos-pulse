//! 註冊與 PKI 的管理操作（`kairos/API.md` 第 11 節）：主控台 API 與 CLI 共用同一份實作。
//!
//! 都是同步的資料庫操作；每個操作寫入 `audit_events`。簽發不在這裡（CLI 不簽發）：核准只把申請標為
//! `approved`，常駐行程簽發（主控台 API 在核准後立即簽發，CLI 核准的由常駐行程在 2 秒內簽發）。

use kairos_proto::enroll::{EnrollMode, EnrollToken, RequestCode, TokenApproval};
use kairos_proto::{HostInfo, is_forbidden_display_char, is_uuid_text, validate_host_info};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};

use super::store::{self, Audit, DbKind, DbState, RequestRow, TokenRow, TokenState};
use crate::per_ip::cidr::Cidr;
use crate::pki::issue::Reason;
use crate::pki::store::Revoke;
use crate::settings;
use crate::store::prelude::*;

/// token 預設有效期間（7 天）與上限（90 天）
pub const DEFAULT_TOKEN_EXPIRY_MS: i64 = 7 * 86_400_000;
pub const MAX_TOKEN_EXPIRY_MS: i64 = 90 * 86_400_000;
/// token 次數上限
pub const MAX_TOKEN_USES: i64 = 100_000;
/// 限定主機的上限
pub const MAX_ALLOWED_AGENT_IDS: usize = 10_000;
/// 批次操作的上限
pub const MAX_BATCH: usize = 1000;
/// 「允許再次自動續簽」最長 90 天
pub const MAX_GRACE_OVERRIDE_MS: i64 = 90 * 86_400_000;
/// 影響數字的確認 token 有效 5 分鐘
pub const IMPACT_WINDOW_MS: i64 = 5 * 60_000;

/// 操作被拒（API 的錯誤碼、CLI 的結束代碼由呼叫端決定）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    /// API 的 `code`（`invalid_parameter`、`token_not_found`、`invalid_state` 等）
    pub code: &'static str,
    /// 哪一個欄位（`details.parameter`）
    pub field: Option<&'static str>,
    pub message: String,
}

impl OpError {
    pub fn invalid(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: "invalid_parameter",
            field: Some(field),
            message: message.into(),
        }
    }

    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            field: None,
            message: message.into(),
        }
    }

    pub fn db(e: impl std::fmt::Display) -> Self {
        Self::new("storage_unavailable", format!("資料庫無法讀寫：{e}"))
    }
}

impl From<rusqlite::Error> for OpError {
    fn from(e: rusqlite::Error) -> Self {
        OpError::db(e)
    }
}

impl From<crate::store::db::StoreError> for OpError {
    fn from(e: crate::store::db::StoreError) -> Self {
        OpError::db(e)
    }
}

/// 建立 token 的輸入（API 的本文、CLI 的選項）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenInput {
    pub name: String,
    pub group: String,
    pub tags: Option<Vec<String>>,
    /// 到期時間（Unix 毫秒）；`None` 為預設 7 天
    pub expires_at: Option<i64>,
    pub max_uses: i64,
    pub approval: Option<TokenApproval>,
    pub allowed_cidrs: Option<Vec<String>>,
    pub batch_mode: bool,
    pub allowed_agent_ids: Option<Vec<String>>,
    pub cert_lifetime_days: Option<i64>,
}

fn check_text(field: &'static str, value: &str) -> Result<(), OpError> {
    let n = value.chars().count();
    if !(1..=64).contains(&n) || value.trim().is_empty() {
        return Err(OpError::invalid(
            field,
            format!("{field} 必須是 1～64 個字元"),
        ));
    }
    if value.chars().any(is_forbidden_display_char) {
        return Err(OpError::invalid(field, format!("{field} 含有不允許的字元")));
    }
    Ok(())
}

/// 檢查建立 token 的輸入（`kairos/API.md` 第 11.2 節）；回傳正規化後的網段與 agent_id
pub fn validate_token_input(input: &TokenInput, now_ms: i64) -> Result<TokenInput, OpError> {
    let mut t = input.clone();
    check_text("name", &t.name)?;
    check_text("group", &t.group)?;
    if let Some(tags) = &t.tags {
        let host = HostInfo {
            tags: Some(tags.clone()),
            ..HostInfo::default()
        };
        validate_host_info(&host).map_err(|e| OpError::invalid("tags", e))?;
    }
    let expires = t.expires_at.unwrap_or(now_ms + DEFAULT_TOKEN_EXPIRY_MS);
    if expires <= now_ms {
        return Err(OpError::invalid("expires_at", "到期時間必須在未來"));
    }
    if expires - now_ms > MAX_TOKEN_EXPIRY_MS {
        return Err(OpError::invalid("expires_at", "token 最長有效 90 天"));
    }
    t.expires_at = Some(expires);
    if !(1..=MAX_TOKEN_USES).contains(&t.max_uses) {
        return Err(OpError::invalid(
            "max_uses",
            "max_uses 必須介於 1..=100000（沒有「無限」）",
        ));
    }
    let approval = t
        .approval
        .ok_or_else(|| OpError::invalid("approval", "approval 必須是 manual 或 auto"))?;
    if let Some(cidrs) = &t.allowed_cidrs {
        let mut norm = Vec::new();
        for c in cidrs {
            let parsed: Cidr = c.trim().parse().map_err(|e| {
                OpError::invalid("allowed_cidrs", format!("網段 \"{c}\" 無效：{e}"))
            })?;
            norm.push(parsed.to_string());
        }
        if norm.is_empty() {
            t.allowed_cidrs = None;
        } else {
            t.allowed_cidrs = Some(norm);
        }
    }
    if let Some(ids) = &t.allowed_agent_ids {
        if ids.len() > MAX_ALLOWED_AGENT_IDS {
            return Err(OpError::invalid(
                "allowed_agent_ids",
                "allowed_agent_ids 最多 10,000 個",
            ));
        }
        let mut norm = Vec::with_capacity(ids.len());
        for id in ids {
            if !is_uuid_text(id) {
                return Err(OpError::invalid(
                    "allowed_agent_ids",
                    format!("\"{id}\" 不是 UUID"),
                ));
            }
            norm.push(id.to_ascii_lowercase());
        }
        t.allowed_agent_ids = (!norm.is_empty()).then_some(norm);
    }
    if t.batch_mode && t.allowed_cidrs.is_none() {
        return Err(OpError::invalid(
            "allowed_cidrs",
            "批次部署模式需要限定來源網段（allowed_cidrs）",
        ));
    }
    if approval == TokenApproval::Auto && t.allowed_cidrs.is_none() && t.allowed_agent_ids.is_none()
    {
        return Err(OpError::invalid(
            "allowed_cidrs",
            "自動核准的 token 需要限定來源網段（allowed_cidrs）或限定主機（allowed_agent_ids）",
        ));
    }
    if let Some(d) = t.cert_lifetime_days
        && !(i64::from(crate::config::MIN_CERT_LIFETIME_DAYS)
            ..=i64::from(crate::config::MAX_CERT_LIFETIME_DAYS))
            .contains(&d)
    {
        return Err(OpError::invalid(
            "cert_lifetime_days",
            "cert_lifetime_days 必須介於 90..=825",
        ));
    }
    Ok(t)
}

/// 建立的 token
#[derive(Debug, Clone)]
pub struct CreatedToken {
    pub row: TokenRow,
    /// 完整的 token（只出現這一次）
    pub token: EnrollToken,
}

fn random_bytes<const N: usize>() -> Result<[u8; N], OpError> {
    let mut b = [0u8; N];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| OpError::new("internal", "無法取得隨機數"))?;
    Ok(b)
}

/// 主控台與 CLI 顯示的 token：只顯示前 4 碼（已決定（2026-10-14））
pub fn token_display(token: &EnrollToken) -> String {
    format!("KRT1-{}…", token.prefix4())
}

/// 建立註冊 token（寫入稽核）
pub fn create_token(
    conn: &mut Connection,
    input: &TokenInput,
    by: &str,
    actor_ip: Option<&str>,
    now_ms: i64,
) -> Result<CreatedToken, OpError> {
    let t = validate_token_input(input, now_ms)?;
    let token = EnrollToken::from_random(&random_bytes::<10>()?);
    let id_bytes = random_bytes::<8>()?;
    let token_id: String = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let row = TokenRow {
        token_id,
        secret_hash: token.lookup_hash().to_vec(),
        hint: token_display(&token),
        name: t.name.clone(),
        group_name: t.group.clone(),
        tags: t.tags.clone(),
        approval: t.approval.unwrap_or(TokenApproval::Manual),
        max_uses: t.max_uses,
        used: 0,
        expires_at: t.expires_at.unwrap_or(now_ms + DEFAULT_TOKEN_EXPIRY_MS),
        allowed_cidrs: t.allowed_cidrs.clone(),
        batch_mode: t.batch_mode,
        allowed_agent_ids: t.allowed_agent_ids.clone(),
        cert_lifetime_days: t.cert_lifetime_days,
        created_at: now_ms,
        created_by: by.to_string(),
        disabled_at: None,
        disabled_by: None,
        disable_reason: None,
        last_used_at: None,
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.insert_token(&row)?;
    tx.audit(
        now_ms,
        &Audit {
            actor: by,
            actor_ip,
            category: "enroll",
            action: "token.create",
            object: Some(&row.token_id),
            result: "ok",
            details: Some(json!({
                "name": row.name,
                "group": row.group_name,
                "approval": store::approval_str(row.approval),
                "max_uses": row.max_uses,
                "expires_at": crate::api::http::ts_ms(row.expires_at),
                "allowed_cidrs": row.allowed_cidrs,
                "batch_mode": row.batch_mode,
                "allowed_agent_ids": row.allowed_agent_ids.as_ref().map(Vec::len),
            })),
            ..Audit::default()
        },
    )?;
    tx.commit()?;
    Ok(CreatedToken { row, token })
}

/// 停用 token（冪等；已簽發的憑證不受影響）
pub fn disable_token(
    conn: &mut Connection,
    token_id: &str,
    by: &str,
    actor_ip: Option<&str>,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<(TokenRow, bool), OpError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some((row, already)) = tx.disable_token(token_id, by, reason, now_ms)? else {
        return Err(OpError::new(
            "token_not_found",
            format!("找不到註冊 token {token_id}"),
        ));
    };
    if !already {
        tx.audit(
            now_ms,
            &Audit {
                actor: by,
                actor_ip,
                category: "enroll",
                action: "token.disable",
                object: Some(token_id),
                result: "ok",
                details: Some(json!({"reason": reason})),
                ..Audit::default()
            },
        )?;
    }
    tx.commit()?;
    Ok((row, already))
}

/// 核准或拒絕的逐筆結果（`kairos/API.md` 第 11.3 節）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 已核准（待簽發）
    Approved,
    Rejected,
    /// 已經決定過（附目前的狀態）
    AlreadyDecided(DbState),
    /// 核准途中已被申請者撤回（不算失敗）
    Withdrawn,
    NotFound,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Approved => "approved",
            Decision::Rejected => "rejected",
            Decision::AlreadyDecided(_) => "already_decided",
            Decision::Withdrawn => "withdrawn",
            Decision::NotFound => "not_found",
        }
    }
}

/// 核准（不簽發）。`lifetime_days` 覆寫這次簽發的有效天數
pub fn approve(
    conn: &mut Connection,
    ids: &[String],
    by: &str,
    actor_ip: Option<&str>,
    lifetime_days: Option<i64>,
    now_ms: i64,
) -> Result<Vec<(String, Decision)>, OpError> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let decision = match tx.get_request(id)? {
            None => Decision::NotFound,
            Some(r) if r.state == DbState::Withdrawn => Decision::Withdrawn,
            Some(r) if r.state != DbState::PendingApproval => Decision::AlreadyDecided(r.state),
            Some(_) => {
                tx.approve(id, by, lifetime_days, now_ms)?;
                Decision::Approved
            }
        };
        tx.audit(
            now_ms,
            &Audit {
                actor: by,
                actor_ip,
                category: "enroll",
                action: "request.approve",
                object: Some(id),
                result: if decision == Decision::Approved {
                    "ok"
                } else {
                    "rejected"
                },
                details: Some(
                    json!({"result": decision.as_str(), "cert_lifetime_days": lifetime_days}),
                ),
                ..Audit::default()
            },
        )?;
        tx.commit()?;
        out.push((id.clone(), decision));
    }
    Ok(out)
}

/// 拒絕（L2；`reason` 會在 Pulse 端顯示）
pub fn reject(
    conn: &mut Connection,
    ids: &[String],
    by: &str,
    actor_ip: Option<&str>,
    code: &str,
    reason: &str,
    now_ms: i64,
) -> Result<Vec<(String, Decision)>, OpError> {
    if !matches!(
        code,
        "unknown_host" | "source_mismatch" | "duplicate" | "other"
    ) {
        return Err(OpError::invalid(
            "reason_code",
            "reason_code 必須是 unknown_host、source_mismatch、duplicate 或 other",
        ));
    }
    if reason.trim().is_empty() || reason.chars().count() > 500 {
        return Err(OpError::invalid("reason", "reason 必填（最多 500 個字元）"));
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let decision = match tx.get_request(id)? {
            None => Decision::NotFound,
            Some(r) if r.state == DbState::Withdrawn => Decision::Withdrawn,
            Some(r) if !r.state.is_pending() => Decision::AlreadyDecided(r.state),
            Some(_) => {
                tx.reject(id, by, code, reason, now_ms)?;
                Decision::Rejected
            }
        };
        tx.audit(
            now_ms,
            &Audit {
                actor: by,
                actor_ip,
                category: "enroll",
                action: "request.reject",
                object: Some(id),
                result: if decision == Decision::Rejected {
                    "ok"
                } else {
                    "rejected"
                },
                details: Some(
                    json!({"result": decision.as_str(), "reason_code": code, "reason": reason}),
                ),
                ..Audit::default()
            },
        )?;
        tx.commit()?;
        out.push((id.clone(), decision));
    }
    Ok(out)
}

/// 撤銷的逐筆結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeResult {
    Revoked { ca_id: Option<i64> },
    Already,
    NotFound,
}

/// 撤銷憑證（立即寫入；常駐行程在 2 秒內重新產生 CRL 並斷開仍在線的連線）
pub fn revoke(
    conn: &mut Connection,
    serials: &[String],
    reason: Reason,
    comment: &str,
    by: &str,
    actor_ip: Option<&str>,
    now_ms: i64,
) -> Result<Vec<(String, RevokeResult)>, OpError> {
    let mut out = Vec::with_capacity(serials.len());
    for serial in serials {
        let serial = serial.trim().to_ascii_uppercase();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = match tx.revoke(&serial, now_ms, now_ms, reason, by, Some(comment))? {
            Revoke::Now(c) => RevokeResult::Revoked { ca_id: c.ca_id },
            Revoke::Already(_) => RevokeResult::Already,
            Revoke::NotFound => RevokeResult::NotFound,
        };
        tx.audit(
            now_ms,
            &Audit {
                actor: by,
                actor_ip,
                category: "pki",
                action: "cert.revoke",
                object: Some(&serial),
                result: if matches!(result, RevokeResult::Revoked { .. }) {
                    "ok"
                } else {
                    "rejected"
                },
                details: Some(json!({"reason": reason.as_str(), "comment": comment})),
                ..Audit::default()
            },
        )?;
        tx.commit()?;
        out.push((serial, result));
    }
    Ok(out)
}

/// 切換模式的影響數字（設計包 `enroll-pause.md` §4.3）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impact {
    /// 寬限期內（關閉後一定無法自動回復）
    pub n: i64,
    /// 30 天內進入寬限期，且目前離線
    pub m: i64,
    /// 長期離線（參考）
    pub l: i64,
    /// 待核准的新註冊申請
    pub r: i64,
    /// 有效 token
    pub t: i64,
}

impl Impact {
    /// 切換到 `mode` 的確認等級與 L2 的確認字串
    pub fn level(&self, mode: EnrollMode) -> (&'static str, Option<String>) {
        if mode == EnrollMode::Closed && self.n + self.m > 0 {
            ("L2", Some(format!("停止續簽 {} 台", self.n + self.m)))
        } else {
            ("L1", None)
        }
    }

    /// 數字的雜湊（`impact_token`；5 分鐘的時間窗）
    pub fn token(&self, mode: EnrollMode, window: i64) -> String {
        let text = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            mode.as_str(),
            self.n,
            self.m,
            self.l,
            self.r,
            self.t,
            window
        );
        kairos_proto::enroll::sha256_hex(text.as_bytes())[..24].to_string()
    }

    /// `impact_token` 是否仍符合目前的數字（目前或前一個時間窗）
    pub fn verify(&self, mode: EnrollMode, token: &str, now_ms: i64) -> bool {
        let w = now_ms / IMPACT_WINDOW_MS;
        token == self.token(mode, w) || token == self.token(mode, w - 1)
    }

    pub fn json(&self, mode: EnrollMode, now_ms: i64) -> Value {
        let (level, confirm) = self.level(mode);
        json!({
            "mode": mode.as_str(),
            "N": self.n, "M": self.m, "L": self.l, "R": self.r, "T": self.t,
            "level": level,
            "confirm_text": confirm,
            "impact_token": self.token(mode, now_ms / IMPACT_WINDOW_MS),
        })
    }
}

/// 計算影響數字（只計 Kairos 簽發紀錄中有最新憑證的主機；手動簽發且未匯入的不在內）
pub fn impact(conn: &Connection, grace_days: u32, now_ms: i64) -> Result<Impact, OpError> {
    let grace_ms = i64::from(grace_days) * 86_400_000;
    let mut stmt = conn.prepare(
        "SELECT c.not_after, h.grace_override_until, h.online, h.last_seen
         FROM hosts h JOIN pki_certificates c ON c.serial = h.current_serial
         WHERE h.lifecycle = 'active' AND c.revoked_at IS NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<i64>>(1)?,
            r.get::<_, i64>(2)? != 0,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let (mut n, mut m, mut l) = (0, 0, 0);
    for row in rows {
        let (not_after, override_until, online, last_seen) = row?;
        if not_after <= now_ms {
            let in_grace =
                now_ms < not_after + grace_ms || override_until.is_some_and(|u| u > now_ms);
            if in_grace {
                n += 1;
            }
        } else if not_after - now_ms <= 30 * 86_400_000 && !online {
            m += 1;
        } else if !online && now_ms - last_seen >= 30 * 86_400_000 {
            l += 1;
        }
    }
    let r: i64 = conn.query_row(
        "SELECT count(*) FROM enroll_requests WHERE kind = 'enroll' AND state IN ('pending_approval', 'approved')",
        [],
        |r| r.get(0),
    )?;
    let t = conn
        .list_tokens()?
        .iter()
        .filter(|t| t.state(now_ms) == TokenState::Active)
        .count() as i64;
    Ok(Impact { n, m, l, r, t })
}

/// 最早受影響的主機最多列出幾台（設計包 S14「列出 K 台與最早受影響的主機」）
pub const GRACE_IMPACT_LIST: usize = 10;

/// 縮短過期寬限期的影響（設計包 `screens.md` S14「儲存過期寬限期時的確認」、`README.md` D11）。
///
/// 寬限期在續簽當下判斷，縮短會立即影響已過期的主機：**K** ＝目前在寬限期內、以新的寬限期計算已經結束、
/// 也沒有個別延長（「允許再次自動續簽」）的主機。K＞0 時是 L2（輸入「停止續簽 K 台」，原因必填）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraceImpact {
    pub from_days: u32,
    pub to_days: u32,
    pub k: i64,
    /// 最早受影響的主機（依原本寬限期的結束時間排序，最多 [`GRACE_IMPACT_LIST`] 台）：
    /// (agent_id, 主機名稱, 憑證到期, 原本寬限期的結束)
    pub earliest: Vec<(String, String, i64, i64)>,
}

/// 計算把寬限期由 `from_days` 改為 `to_days` 的影響（不是縮短時 K＝0）
pub fn grace_impact(
    conn: &Connection,
    from_days: u32,
    to_days: u32,
    now_ms: i64,
) -> Result<GraceImpact, OpError> {
    let mut out = GraceImpact {
        from_days,
        to_days,
        k: 0,
        earliest: Vec::new(),
    };
    if to_days >= from_days {
        return Ok(out);
    }
    let (from_ms, to_ms) = (
        i64::from(from_days) * 86_400_000,
        i64::from(to_days) * 86_400_000,
    );
    let mut stmt = conn.prepare(
        "SELECT h.agent_id, COALESCE(NULLIF(h.display_name, ''), h.hostname), c.not_after, h.grace_override_until
         FROM hosts h JOIN pki_certificates c ON c.serial = h.current_serial
         WHERE h.lifecycle = 'active' AND c.revoked_at IS NULL AND c.not_after <= ?1",
    )?;
    let rows = stmt.query_map([now_ms], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let mut hit = Vec::new();
    for row in rows {
        let (agent_id, name, not_after, override_until) = row?;
        if override_until.is_some_and(|u| u > now_ms) {
            continue;
        }
        let in_grace_now = now_ms < not_after + from_ms;
        let in_grace_after = now_ms < not_after + to_ms;
        if in_grace_now && !in_grace_after {
            hit.push((agent_id, name, not_after, not_after + from_ms));
        }
    }
    hit.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| a.0.cmp(&b.0)));
    out.k = hit.len() as i64;
    hit.truncate(GRACE_IMPACT_LIST);
    out.earliest = hit;
    Ok(out)
}

/// 會讓主機無法續簽的設定變更（關閉註冊端點、縮短寬限期）的影響與確認 token。
///
/// `PUT /api/v1/pki/policy` 與 `POST /api/v1/settings/reset`（「改用設定檔的值」）共用：送出前以
/// `GET …/impact` 取得數字與 `impact_token`，送出時伺服器重新計算，不符時 409 `impact_changed`（同第 11.4 節）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangeImpact {
    /// 註冊端點模式由非關閉改為關閉時：(原本的模式, 切換到關閉的影響)
    pub closing: Option<(EnrollMode, Impact)>,
    /// 寬限期縮短時
    pub grace: Option<GraceImpact>,
}

impl ChangeImpact {
    /// 是否需要 `impact_token`（關閉註冊端點或縮短寬限期）
    pub fn required(&self) -> bool {
        self.closing.is_some() || self.grace.as_ref().is_some_and(|g| g.to_days < g.from_days)
    }

    /// 會停止續簽的台數：關閉時 N＋M（寬限期內的主機已包含在 N 中），否則 K
    pub fn count(&self) -> i64 {
        match (&self.closing, &self.grace) {
            (Some((_, i)), _) => i.n + i.m,
            (None, Some(g)) => g.k,
            (None, None) => 0,
        }
    }

    /// 確認等級與 L2 的確認字串（「停止續簽 K 台」，與 S21 關閉註冊端點相同）
    pub fn level(&self) -> (&'static str, Option<String>) {
        let n = self.count();
        if n > 0 {
            ("L2", Some(format!("停止續簽 {n} 台")))
        } else {
            ("L1", None)
        }
    }

    pub fn token(&self, window: i64) -> String {
        let closing = self.closing.as_ref().map_or_else(String::new, |(from, i)| {
            format!("{}>{}|{}|{}|{}|{}", from.as_str(), i.n, i.m, i.l, i.r, i.t)
        });
        let grace = self.grace.as_ref().map_or_else(String::new, |g| {
            format!("{}>{}|{}", g.from_days, g.to_days, g.k)
        });
        let text = format!("change|{closing}|{grace}|{window}");
        kairos_proto::enroll::sha256_hex(text.as_bytes())[..24].to_string()
    }

    /// `impact_token` 是否仍符合目前的數字（目前或前一個時間窗）
    pub fn verify(&self, token: &str, now_ms: i64) -> bool {
        let w = now_ms / IMPACT_WINDOW_MS;
        token == self.token(w) || token == self.token(w - 1)
    }

    pub fn json(&self, now_ms: i64) -> Value {
        let (level, confirm) = self.level();
        json!({
            "required": self.required(),
            "count": self.count(),
            "level": level,
            "confirm_text": confirm,
            "impact_token": self.token(now_ms / IMPACT_WINDOW_MS),
            "mode": self.closing.as_ref().map(|(from, i)| json!({
                "from": from.as_str(),
                "to": "closed",
                "N": i.n, "M": i.m, "L": i.l, "R": i.r, "T": i.t,
            })),
            "grace": self.grace.as_ref().map(|g| json!({
                "from_days": g.from_days,
                "to_days": g.to_days,
                "K": g.k,
                "earliest": g.earliest.iter().map(|(id, name, not_after, ends)| json!({
                    "agent_id": id,
                    "hostname": name,
                    "not_after": crate::api::http::ts_ms(*not_after),
                    "grace_ends_at": crate::api::http::ts_ms(*ends),
                })).collect::<Vec<_>>(),
            })),
        })
    }

    /// 稽核紀錄的 `impact`
    pub fn audit_json(&self) -> Value {
        json!({
            "mode": self.closing.as_ref().map(|(from, i)| json!({"from": from.as_str(), "N": i.n, "M": i.m, "L": i.l, "R": i.r, "T": i.t})),
            "grace": self.grace.as_ref().map(|g| json!({"from_days": g.from_days, "to_days": g.to_days, "K": g.k})),
        })
    }
}

/// 確認過的影響（寫入稽核：等級、原因、確認時的數字）
#[derive(Debug, Clone, Copy)]
pub struct Confirmed<'a> {
    pub level: &'a str,
    pub reason: Option<&'a str>,
    pub impact: &'a ChangeImpact,
}

impl Confirmed<'_> {
    fn add_to(&self, d: &mut serde_json::Map<String, Value>) {
        d.insert("level".into(), json!(self.level));
        d.insert("reason".into(), json!(self.reason));
        d.insert("impact".into(), self.impact.audit_json());
    }
}

/// 由目前與變更後的生效值計算影響（關閉註冊端點、縮短寬限期；其他變更沒有影響）
pub fn change_impact(
    conn: &Connection,
    before: &settings::Effective,
    after: &settings::Effective,
    now_ms: i64,
) -> Result<ChangeImpact, OpError> {
    let mut out = ChangeImpact::default();
    if after.mode == EnrollMode::Closed && before.mode != EnrollMode::Closed {
        out.closing = Some((
            before.mode,
            impact(conn, before.policy.expired_renew_grace_days, now_ms)?,
        ));
    }
    let (from, to) = (
        before.policy.expired_renew_grace_days,
        after.policy.expired_renew_grace_days,
    );
    if to < from {
        out.grace = Some(grace_impact(conn, from, to, now_ms)?);
    }
    Ok(out)
}

/// 修改模式與自動暫停（寫入 `settings` 與稽核）。`expected` 為 L2 時確認過的影響數字
#[allow(clippy::too_many_arguments)]
pub fn set_mode(
    conn: &mut Connection,
    mode: Option<EnrollMode>,
    auto_pause_enabled: Option<bool>,
    auto_pause_after_hours: Option<u32>,
    by: &str,
    actor_ip: Option<&str>,
    reason: Option<&str>,
    impact: &Impact,
    level: &str,
    now_ms: i64,
) -> Result<(), OpError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = settings::overrides_from(&tx.load_settings()?);
    let mut changes = serde_json::Map::new();
    if let Some(m) = mode {
        let before = current.get(settings::KEY_MODE).cloned();
        tx.set_setting(settings::KEY_MODE, &json!(m.as_str()), by, now_ms)?;
        if m == EnrollMode::Open && before.as_ref().and_then(|v| v.as_str()) != Some("open") {
            tx.set_setting(settings::KEY_OPEN_SINCE, &json!(now_ms), by, now_ms)?;
        }
        changes.insert("mode".into(), json!({"from": before, "to": m.as_str()}));
    }
    if let Some(v) = auto_pause_enabled {
        tx.set_setting(settings::KEY_AUTO_PAUSE_ENABLED, &json!(v), by, now_ms)?;
        changes.insert("auto_pause_enabled".into(), json!(v));
    }
    if let Some(h) = auto_pause_after_hours {
        if !(1..=168).contains(&h) {
            return Err(OpError::invalid(
                "auto_pause_after_hours",
                "auto_pause_after_hours 必須介於 1..=168",
            ));
        }
        tx.set_setting(settings::KEY_AUTO_PAUSE_AFTER_HOURS, &json!(h), by, now_ms)?;
        changes.insert("auto_pause_after_hours".into(), json!(h));
    }
    tx.audit(now_ms,
        &Audit {
            actor: by,
            actor_ip,
            category: "settings",
            action: "mode.change",
            object: Some("enroll.mode"),
            result: "ok",
            details: Some(json!({
                "changes": changes,
                "reason": reason,
                "level": level,
                "impact": {"N": impact.n, "M": impact.m, "L": impact.l, "R": impact.r, "T": impact.t},
            })),
            ..Audit::default()
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// 修改憑證政策（`settings` 表）
pub fn set_policy(
    conn: &mut Connection,
    values: &[(&'static str, Value)],
    by: &str,
    actor_ip: Option<&str>,
    confirmed: Option<&Confirmed<'_>>,
    now_ms: i64,
) -> Result<(), OpError> {
    for (k, v) in values {
        settings::validate_override(k, v).map_err(|e| OpError::invalid("policy", e))?;
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (k, v) in values {
        tx.set_setting(k, v, by, now_ms)?;
    }
    tx.audit(
        now_ms,
        &Audit {
            actor: by,
            actor_ip,
            category: "settings",
            action: "policy.change",
            object: Some("pki.policy"),
            result: "ok",
            details: Some({
                let mut d: serde_json::Map<String, Value> = values
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.clone()))
                    .collect();
                if let Some(c) = confirmed {
                    c.add_to(&mut d);
                }
                Value::Object(d)
            }),
            ..Audit::default()
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// 改用設定檔的值：刪除 `settings` 中的鍵（L1、稽核）。回傳 (鍵, 先前是否由主控台管理)
pub fn reset_settings(
    conn: &mut Connection,
    keys: &[String],
    by: &str,
    actor_ip: Option<&str>,
    confirmed: Option<&Confirmed<'_>>,
    now_ms: i64,
) -> Result<Vec<(String, bool)>, OpError> {
    for k in keys {
        if !settings::CONSOLE_KEYS.contains(&k.as_str()) {
            return Err(OpError::invalid(
                "keys",
                format!(
                    "{k} 不是可從主控台修改的設定（{}）",
                    settings::CONSOLE_KEYS.join("、")
                ),
            ));
        }
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut out = Vec::new();
    for k in keys {
        let existed = tx.delete_setting(k)?;
        tx.audit(
            now_ms,
            &Audit {
                actor: by,
                actor_ip,
                category: "settings",
                action: "setting.reset",
                object: Some(k),
                result: if existed { "ok" } else { "rejected" },
                details: Some({
                    let mut d = serde_json::Map::new();
                    d.insert("existed".into(), json!(existed));
                    if let Some(c) = confirmed {
                        c.add_to(&mut d);
                    }
                    Value::Object(d)
                }),
                ..Audit::default()
            },
        )?;
        out.push((k.clone(), existed));
    }
    tx.commit()?;
    Ok(out)
}

/// 允許再次自動續簽（S14，D12）
pub fn grace_override(
    conn: &mut Connection,
    agent_ids: &[String],
    until_ms: i64,
    by: &str,
    actor_ip: Option<&str>,
    reason: &str,
    now_ms: i64,
) -> Result<Vec<(String, bool)>, OpError> {
    if until_ms <= now_ms || until_ms - now_ms > MAX_GRACE_OVERRIDE_MS {
        return Err(OpError::invalid("until", "until 必須在未來 90 天以內"));
    }
    if reason.trim().is_empty() {
        return Err(OpError::invalid("reason", "reason 必填"));
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut out = Vec::new();
    for id in agent_ids {
        let id = id.to_ascii_lowercase();
        let found = tx.set_grace_override(&id, Some(until_ms))?;
        tx.audit(
            now_ms,
            &Audit {
                actor: by,
                actor_ip,
                category: "pki",
                action: "grace.override",
                object: Some(&id),
                result: if found { "ok" } else { "rejected" },
                details: Some(
                    json!({"until": crate::api::http::ts_ms(until_ms), "reason": reason}),
                ),
                ..Audit::default()
            },
        )?;
        out.push((id, found));
    }
    tx.commit()?;
    Ok(out)
}

/// 以 `request_id` 或申請編號找申請
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    One(Box<RequestRow>),
    NotFound,
    /// 申請編號對到多筆（40 位元不保證唯一）
    Ambiguous(Vec<RequestRow>),
}

pub fn resolve_request(conn: &Connection, input: &str) -> Result<Resolved, OpError> {
    let input = input.trim();
    if is_uuid_text(input) {
        return Ok(match conn.get_request(&input.to_ascii_lowercase())? {
            Some(r) => Resolved::One(Box::new(r)),
            None => Resolved::NotFound,
        });
    }
    let Some(code) = RequestCode::parse(input) else {
        return Ok(Resolved::NotFound);
    };
    let mut rows = conn.requests_by_code(&code.compact())?;
    // 有待核准的申請時只看待核准的
    if rows.iter().any(|r| r.state.is_pending()) {
        rows.retain(|r| r.state.is_pending());
    }
    Ok(match rows.len() {
        0 => Resolved::NotFound,
        1 => Resolved::One(Box::new(rows.remove(0))),
        _ => Resolved::Ambiguous(rows),
    })
}

/// 申請的 JSON（主控台列表、詳情與 CLI 的 `--json`）
pub fn request_json(r: &RequestRow) -> Value {
    let (state, detail) = r.state.protocol();
    let t = |ms: Option<i64>| ms.map(crate::api::http::ts_ms);
    let code = RequestCode::parse(&r.request_code)
        .map(|c| c.to_string())
        .unwrap_or_else(|| r.request_code.clone());
    json!({
        "request_id": r.request_id,
        "request_code": code,
        "kind": r.kind.as_str(),
        "state": r.state.as_str(),
        "protocol_state": state,
        "approval_detail": detail,
        "agent_id": r.agent_id,
        "hostname": r.hostname,
        "display_name": r.display_name,
        "tags": r.tags,
        "os": r.os.as_deref().and_then(|o| serde_json::from_str::<Value>(o).ok()),
        "source_ip": r.source_ip,
        "token_id": r.token_id,
        "host_fingerprint": r.host_fingerprint,
        "pulse_version": r.pulse_version,
        "server_verification": r.server_verification,
        "flags": r.flags,
        "spki_fingerprint_short": kairos_proto::enroll::hex_lower(&r.spki_sha256)[..8].to_ascii_uppercase(),
        "previous_serial": r.previous_serial,
        "expired_serial": r.expired_serial,
        "cert_lifetime_days": r.cert_lifetime_days,
        "created_at": t(Some(r.created_at)),
        "expires_at": t(Some(r.expires_at)),
        "decided_at": t(r.decided_at),
        "decided_by": r.decided_by,
        "reject_reason_code": r.reject_reason_code,
        "reject_reason": r.reject_reason,
        "issued_serial": r.issued_serial,
        "issue_error": r.issue_error,
        "delivered_at": t(r.delivered_at),
        "withdrawn_at": t(r.withdrawn_at),
        "withdraw_reason": r.withdraw_reason,
        "withdraw_initiated_by": r.withdraw_initiated_by,
        "withdraw_ip": r.withdraw_ip,
        "closed_at": t(r.closed_at),
    })
}

/// token 的 JSON（不含秘密與雜湊；`token` 只在建立時另外加入）
pub fn token_json(t: &TokenRow, now_ms: i64) -> Value {
    json!({
        "token_id": t.token_id,
        "hint": t.hint,
        "token_hint": t.hint,
        "name": t.name,
        "group": t.group_name,
        "tags": t.tags,
        "approval": store::approval_str(t.approval),
        "state": t.state(now_ms).as_str(),
        "max_uses": t.max_uses,
        "used": t.used,
        "remaining_uses": t.remaining_uses(),
        "expires_at": crate::api::http::ts_ms(t.expires_at),
        "allowed_cidrs": t.allowed_cidrs,
        "batch_mode": t.batch_mode,
        "allowed_agent_ids": t.allowed_agent_ids,
        "cert_lifetime_days": t.cert_lifetime_days,
        "created_at": crate::api::http::ts_ms(t.created_at),
        "created_by": t.created_by,
        "disabled_at": t.disabled_at.map(crate::api::http::ts_ms),
        "disabled_by": t.disabled_by,
        "disable_reason": t.disable_reason,
        "last_used_at": t.last_used_at.map(crate::api::http::ts_ms),
    })
}

/// 部署進度（漏斗；`queued`、`clock_rejected` 由常駐行程的記憶體提供）
pub fn token_progress(conn: &Connection, token_id: &str) -> Result<Value, OpError> {
    let mut stmt = conn.prepare(
        "SELECT r.state, count(*), sum(CASE WHEN h.connected_at IS NOT NULL AND h.connected_at >= r.decided_at THEN 1 ELSE 0 END)
         FROM enroll_requests r LEFT JOIN hosts h ON h.agent_id = r.agent_id
         WHERE r.token_id = ?1 GROUP BY r.state",
    )?;
    let mut counts = std::collections::HashMap::new();
    let mut online = 0i64;
    for row in stmt.query_map([token_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<i64>>(2)?,
        ))
    })? {
        let (state, n, on) = row?;
        if state == "issued" {
            online = on.unwrap_or(0);
        }
        counts.insert(state, n);
    }
    let get = |k: &str| counts.get(k).copied().unwrap_or(0);
    let issued = get("issued");
    Ok(json!({
        "used": counts.values().sum::<i64>(),
        "pending": get("pending_approval") + get("approved"),
        "rejected": get("rejected"),
        "withdrawn": get("withdrawn"),
        "expired": get("request_expired"),
        "replaced": get("replaced"),
        "issued": issued,
        "online": online,
        "issued_not_online": (issued - online).max(0),
    }))
}

/// 申請種類（API 的篩選）
pub fn parse_kind(s: &str) -> Option<DbKind> {
    DbKind::parse(s)
}

#[cfg(test)]
mod impact_tests {
    use super::*;
    use crate::store::sqlite::{MIGRATIONS, migrate};

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 1_000 * DAY;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        conn
    }

    /// 一台主機與它目前的憑證（`expired_days`：已過期幾天，負值＝還有幾天到期）
    fn host(conn: &Connection, n: u8, expired_days: i64, override_until: Option<i64>) {
        let agent_id = format!("00000000-0000-4000-8000-0000000000{n:02}");
        let serial = format!("S{n:02}");
        conn.execute(
            "INSERT INTO hosts (agent_id, hostname, first_seen, last_seen, current_serial, grace_override_until)
             VALUES (?1, ?2, 0, ?3, ?4, ?5)",
            rusqlite::params![agent_id, format!("host-{n:02}"), NOW, serial, override_until],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pki_certificates (serial, agent_id, spki_sha256, not_before, not_after, cert_der, issued_at, issued_via)
             VALUES (?1, ?2, x'00', 0, ?3, x'00', 0, 'test')",
            rusqlite::params![serial, agent_id, NOW - expired_days * DAY],
        )
        .unwrap();
    }

    fn eff(mode: EnrollMode, grace: u32) -> settings::Effective {
        let mut config = crate::config::AppConfig::default();
        config.pki.policy.expired_renew_grace_days = grace;
        let mut e = settings::effective(&config, &settings::Overrides::new());
        e.mode = mode;
        e
    }

    #[test]
    fn shortening_the_grace_period_counts_hosts_that_stop_renewing() {
        let conn = db();
        host(&conn, 1, 5, None); // 寬限期 90 → 30：仍在寬限期內
        host(&conn, 2, 45, None); // 立即無法續簽
        host(&conn, 3, 60, None); // 立即無法續簽（較早結束）
        host(&conn, 4, 45, Some(NOW + 10 * DAY)); // 個別延長：不受影響
        host(&conn, 5, 120, None); // 原本就超過寬限期
        host(&conn, 6, -10, None); // 尚未過期
        let g = grace_impact(&conn, 90, 30, NOW).unwrap();
        assert_eq!(g.k, 2);
        // 依原本寬限期的結束排序：60 天前過期的較早結束
        assert_eq!(g.earliest[0].1, "host-03");
        assert_eq!(g.earliest[1].1, "host-02");
        assert_eq!(g.earliest[0].3, NOW - 60 * DAY + 90 * DAY);
        // 延長或不變：沒有影響
        assert_eq!(grace_impact(&conn, 30, 90, NOW).unwrap().k, 0);
        assert_eq!(grace_impact(&conn, 90, 90, NOW).unwrap().k, 0);
        // 改為 0：寬限期內的 4 台中，個別延長的那台以外都受影響
        assert_eq!(grace_impact(&conn, 90, 0, NOW).unwrap().k, 3);
    }

    #[test]
    fn change_impact_levels_tokens_and_confirm_text() {
        let conn = db();
        host(&conn, 1, 45, None);
        // 沒有關閉、沒有縮短：不需要確認
        let none = change_impact(
            &conn,
            &eff(EnrollMode::Open, 90),
            &eff(EnrollMode::RenewOnly, 120),
            NOW,
        )
        .unwrap();
        assert!(!none.required());
        assert_eq!(none.level(), ("L1", None));
        // 縮短寬限期：K＝1 → L2「停止續簽 1 台」
        let grace = change_impact(
            &conn,
            &eff(EnrollMode::Open, 90),
            &eff(EnrollMode::Open, 30),
            NOW,
        )
        .unwrap();
        assert!(grace.required());
        assert_eq!(grace.count(), 1);
        assert_eq!(grace.level(), ("L2", Some("停止續簽 1 台".to_string())));
        // 縮短但沒有主機受影響：仍需要 impact_token，等級為 L1
        let grace_l1 = change_impact(
            &conn,
            &eff(EnrollMode::Open, 90),
            &eff(EnrollMode::Open, 60),
            NOW,
        )
        .unwrap();
        assert!(grace_l1.required());
        assert_eq!(grace_l1.level().0, "L1");
        // 關閉：寬限期內的主機（N）計入；同時縮短時以 N＋M 為準
        let closing = change_impact(
            &conn,
            &eff(EnrollMode::Open, 90),
            &eff(EnrollMode::Closed, 30),
            NOW,
        )
        .unwrap();
        assert_eq!(
            closing.closing.as_ref().map(|(m, i)| (*m, i.n)),
            Some((EnrollMode::Open, 1))
        );
        assert_eq!(closing.count(), 1);
        // token：同一時間窗與前一個時間窗有效，數字改變後失效
        let tok = grace.token(NOW / IMPACT_WINDOW_MS);
        assert!(grace.verify(&tok, NOW));
        assert!(grace.verify(&tok, NOW + IMPACT_WINDOW_MS));
        assert!(!grace.verify(&tok, NOW + 2 * IMPACT_WINDOW_MS));
        assert!(!grace_l1.verify(&tok, NOW));
        host(&conn, 2, 50, None);
        let changed = change_impact(
            &conn,
            &eff(EnrollMode::Open, 90),
            &eff(EnrollMode::Open, 30),
            NOW,
        )
        .unwrap();
        assert_eq!(changed.count(), 2);
        assert!(!changed.verify(&tok, NOW));
        let j = changed.json(NOW);
        assert_eq!(j["level"], "L2");
        assert_eq!(j["confirm_text"], "停止續簽 2 台");
        assert_eq!(j["grace"]["K"], 2);
        assert_eq!(j["grace"]["earliest"][0]["hostname"], "host-02");
        assert!(j["mode"].is_null());
    }
}
