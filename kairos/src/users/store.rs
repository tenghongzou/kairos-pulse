//! `users`、`user_recovery_codes`、`role_assignments`、`sessions` 的讀寫（schema v6）。
//!
//! 都是同步的資料庫操作，主控台與 CLI 共用；稽核由呼叫端在同一個交易中寫入。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use crate::api::http::ts_ms;
use crate::rbac::{Assignment, Grants};
use crate::store::db::StoreResult;
use serde_json::{Value, json};

/// 帳號名稱的長度上限
pub const MAX_USERNAME_CHARS: usize = 64;

/// 帳號名稱：1～64 個字元；英數字與 `.`、`_`、`-`、`@`（LDAP 帳號另外允許 `\` 以外的可見字元）
pub fn validate_username(name: &str) -> Result<(), String> {
    let n = name.chars().count();
    if n == 0 || n > MAX_USERNAME_CHARS {
        return Err(format!("帳號必須是 1～{MAX_USERNAME_CHARS} 個字元"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
    {
        return Err("帳號只能包含英文字母、數字與 . _ - @".to_string());
    }
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Err("帳號必須以英文字母或數字開頭".to_string());
    }
    Ok(())
}

/// 顯示名稱、Email 等自由文字：最多 128 個字元、不含控制字元
pub fn validate_text(field: &str, v: &str) -> Result<(), String> {
    if v.chars().count() > 128 || v.chars().any(char::is_control) {
        return Err(format!("{field} 最多 128 個字元，且不可含控制字元"));
    }
    Ok(())
}

/// 帳號的來源
pub const SOURCE_LOCAL: &str = "local";

pub const SOURCE_LDAP: &str = "ldap";

/// `users` 的一列（不含密碼雜湊與 TOTP 秘密）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRow {
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub source: String,
    pub ldap_dn: Option<String>,
    pub has_password: bool,
    pub password_changed_at: Option<i64>,
    pub must_change_password: bool,
    pub totp_enabled_at: Option<i64>,
    pub state: String,
    pub failed_logins: i64,
    pub locked_until: Option<i64>,
    pub last_login_at: Option<i64>,
    pub last_login_ip: Option<String>,
    pub created_at: i64,
    pub created_by: String,
    pub disabled_at: Option<i64>,
    pub disabled_by: Option<String>,
    pub disable_reason: Option<String>,
}

impl UserRow {
    pub fn active(&self) -> bool {
        self.state == "active"
    }

    pub fn locked(&self, now_ms: i64) -> bool {
        self.locked_until.is_some_and(|t| t > now_ms)
    }

    /// 狀態：`active`、`disabled`、`locked`（S16 的篩選）
    pub fn status(&self, now_ms: i64) -> &'static str {
        if !self.active() {
            "disabled"
        } else if self.locked(now_ms) {
            "locked"
        } else {
            "active"
        }
    }

    /// API 與 CLI `--json` 的表示（不含任何秘密）
    pub fn json(&self, now_ms: i64) -> Value {
        json!({
            "user_id": self.user_id,
            "username": self.username,
            "display_name": self.display_name,
            "email": self.email,
            "source": self.source,
            "ldap_dn": self.ldap_dn,
            "status": self.status(now_ms),
            "mfa": if self.totp_enabled_at.is_some() { "totp" } else if self.source == SOURCE_LDAP { "directory" } else { "none" },
            "must_change_password": self.must_change_password,
            "password_changed_at": self.password_changed_at.map(ts_ms),
            "locked_until": self.locked_until.filter(|t| *t > now_ms).map(ts_ms),
            "last_login_at": self.last_login_at.map(ts_ms),
            "last_login_ip": self.last_login_ip,
            "created_at": ts_ms(self.created_at),
            "created_by": self.created_by,
            "disabled_at": self.disabled_at.map(ts_ms),
            "disabled_by": self.disabled_by,
            "disable_reason": self.disable_reason,
        })
    }
}

/// 新使用者
#[derive(Debug, Clone, Default)]
pub struct NewUser<'a> {
    pub username: &'a str,
    pub display_name: Option<&'a str>,
    pub email: Option<&'a str>,
    pub source: &'a str,
    pub ldap_dn: Option<&'a str>,
    pub password_hash: Option<&'a str>,
    pub must_change_password: bool,
}

// ---------------------------------------------------------------------------
// 角色指派
// ---------------------------------------------------------------------------

/// 主體：本機或目錄服務的使用者、目錄服務的群組
pub const SUBJECT_USER: &str = "user";

pub const SUBJECT_LDAP_GROUP: &str = "ldap_group";

/// 目錄服務群組的 DN 正規化：小寫、去除 `,` 與 `=` 前後的空白
pub fn normalize_dn(dn: &str) -> String {
    dn.split(',')
        .map(|part| {
            let mut kv = part.splitn(2, '=');
            let k = kv.next().unwrap_or("").trim();
            match kv.next() {
                Some(v) => format!("{}={}", k, v.trim()),
                None => k.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(",")
        .to_lowercase()
}

/// 角色指派與它的來源（目錄服務群組；個人指派為 `None`）
pub type Sourced = Vec<(Assignment, Option<String>)>;

// ---------------------------------------------------------------------------
// 工作階段
// ---------------------------------------------------------------------------

/// 新的工作階段
#[derive(Debug, Clone)]
pub struct NewSession<'a> {
    pub session_hash: &'a [u8],
    pub public_id: &'a str,
    pub user_id: i64,
    pub method: &'a str,
    pub ldap_groups: &'a [String],
    pub idle_timeout_ms: i64,
    pub expires_at: i64,
    pub ip: &'a str,
    pub user_agent: Option<&'a str>,
}

/// 工作階段的一列
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub public_id: String,
    pub user_id: i64,
    pub method: String,
    pub ldap_groups: Vec<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub idle_timeout_ms: i64,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub ended_at: Option<i64>,
    pub end_reason: Option<String>,
}

impl SessionRow {
    /// 仍有效：沒有結束、沒有超過絕對逾時與閒置逾時
    pub fn valid(&self, now_ms: i64) -> bool {
        self.ended_at.is_none()
            && now_ms < self.expires_at
            && now_ms < self.last_seen_at + self.idle_timeout_ms
    }

    pub fn json(&self, now_ms: i64, current: bool) -> Value {
        json!({
            "session_id": self.public_id,
            "current": current,
            "method": self.method,
            "created_at": ts_ms(self.created_at),
            "last_seen_at": ts_ms(self.last_seen_at),
            "expires_at": ts_ms(self.expires_at),
            "idle_expires_at": ts_ms((self.last_seen_at + self.idle_timeout_ms).min(self.expires_at)),
            "ip": self.ip,
            "user_agent": self.user_agent,
            "active": self.valid(now_ms),
            "ended_at": self.ended_at.map(ts_ms),
            "end_reason": self.end_reason,
        })
    }
}

/// 本機與目錄服務帳號、角色指派、工作階段的儲存（`users`、`user_recovery_codes`、`role_assignments`、
/// `sessions`）。SQLite 的實作在 `store/sqlite/users.rs`
///
/// 需要原子性的多步操作（例如「建立使用者＋指派角色＋稽核」）由呼叫端包在交易中（[`crate::store::db::Store::begin`]
/// 或 [`crate::store::db::in_write_tx`]）。登入失敗計數與 TOTP 的時間步已是條件式或原子的 UPDATE（`kairos/HA.md` 第 5.4 節）
pub trait UserStore {
    fn get_user(&self, user_id: i64) -> StoreResult<Option<UserRow>>;

    /// 以帳號查詢（不分大小寫）
    fn find_user(&self, username: &str) -> StoreResult<Option<UserRow>>;

    fn list_users(&self) -> StoreResult<Vec<UserRow>>;

    /// 密碼雜湊（登入驗證用）
    fn password_hash(&self, user_id: i64) -> StoreResult<Option<String>>;

    /// 建立使用者；帳號已存在時回 `Ok(None)`
    fn create_user(&self, u: &NewUser<'_>, by: &str, now_ms: i64) -> StoreResult<Option<i64>>;

    /// LDAP 使用者第一次登入時建立、之後更新顯示名稱、Email 與 DN；回傳 user_id。
    /// 同名的本機帳號存在時回 `Ok(None)`（不讓目錄服務的帳號接管本機帳號）
    fn upsert_ldap_user(
        &self,
        username: &str,
        dn: &str,
        display_name: Option<&str>,
        email: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<Option<i64>>;

    fn set_password(
        &self,
        user_id: i64,
        hash: &str,
        must_change: bool,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 只更新雜湊（參數改變後的重新雜湊；不改變密碼的時間）。密碼在這之間被變更時不取代
    fn rehash_password(&self, user_id: i64, old: &str, new: &str) -> StoreResult<bool>;

    fn set_profile(
        &self,
        user_id: i64,
        display_name: Option<&str>,
        email: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 停用或啟用；回傳是否有變更
    fn set_state(
        &self,
        user_id: i64,
        active: bool,
        by: &str,
        reason: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 刪除使用者：角色指派、復原碼一併刪除，工作階段結束，擁有的 API token 撤銷
    fn delete_user(&self, user_id: i64, by: &str, now_ms: i64) -> StoreResult<bool>;

    /// 登入成功：清除失敗次數、記下時間與來源
    fn login_succeeded(&self, user_id: i64, ip: &str, now_ms: i64) -> StoreResult<()>;

    /// 登入失敗：累計次數，達到門檻時鎖定並歸零；回傳鎖定到何時（這次鎖定時）
    fn login_failed(
        &self,
        user_id: i64,
        threshold: u32,
        lock_ms: i64,
        now_ms: i64,
    ) -> StoreResult<Option<i64>>;

    /// 解除鎖定
    fn unlock(&self, user_id: i64) -> StoreResult<bool>;

    /// TOTP 的加密秘密與上次使用的時間步：(啟用中的秘密, 綁定中的秘密, 上次的時間步)
    #[allow(clippy::type_complexity)]
    fn totp_state(
        &self,
        user_id: i64,
    ) -> StoreResult<Option<(Option<Vec<u8>>, Option<Vec<u8>>, Option<i64>)>>;

    fn set_totp_pending(&self, user_id: i64, sealed: &[u8]) -> StoreResult<()>;

    /// 啟用綁定中的秘密並換上新的復原碼
    fn enable_totp(
        &self,
        user_id: i64,
        step: i64,
        recovery_hashes: &[Vec<u8>],
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 停用或重設 TOTP（秘密、綁定中的秘密與復原碼一併刪除）；回傳先前是否啟用
    fn clear_totp(&self, user_id: i64, now_ms: i64) -> StoreResult<bool>;

    /// 記下使用過的時間步（只在比上次新時成功；同一個碼不能用兩次）
    fn use_totp_step(&self, user_id: i64, step: i64) -> StoreResult<bool>;

    /// 使用一組復原碼（只能用一次）
    fn use_recovery_code(&self, user_id: i64, hash: &[u8], now_ms: i64) -> StoreResult<bool>;

    fn user_assignments(&self, user_id: i64) -> StoreResult<Vec<Assignment>>;

    /// 使用者的權限：個人指派＋所屬目錄服務群組的對應
    fn grants_for(&self, user_id: i64, ldap_groups: &[String]) -> StoreResult<(Grants, Sourced)>;

    /// 新增一筆指派；已存在時回 `false`
    fn add_assignment(
        &self,
        subject_type: &str,
        subject: &str,
        a: &Assignment,
        by: &str,
        now_ms: i64,
    ) -> StoreResult<bool>;

    fn remove_assignment(
        &self,
        subject_type: &str,
        subject: &str,
        a: &Assignment,
    ) -> StoreResult<bool>;

    /// 以新的一組取代主體的所有指派；回傳 (新增, 移除)
    fn replace_assignments(
        &self,
        subject_type: &str,
        subject: &str,
        new: &[Assignment],
        by: &str,
        now_ms: i64,
    ) -> StoreResult<(Vec<Assignment>, Vec<Assignment>)>;

    /// 目錄服務群組的對應：(群組 DN, 指派)
    fn ldap_mappings(&self) -> StoreResult<Vec<(String, Assignment)>>;

    /// 啟用中、有全站管理員個人指派的使用者數（「不可移除最後一位管理員」）
    fn active_global_admins(&self) -> StoreResult<Vec<i64>>;

    /// 各角色目前的成員數（個人指派，不含群組對應；S16「角色與權限」分頁）
    fn role_member_counts(&self) -> StoreResult<Vec<(String, i64)>>;

    fn create_session(&self, s: &NewSession<'_>, now_ms: i64) -> StoreResult<()>;

    /// 以 cookie 秘密的雜湊查詢
    fn session_by_hash(&self, hash: &[u8]) -> StoreResult<Option<SessionRow>>;

    /// 使用者的工作階段（`active_only` 時只列有效的；新到舊）
    fn user_sessions(
        &self,
        user_id: i64,
        active_only: bool,
        now_ms: i64,
    ) -> StoreResult<Vec<SessionRow>>;

    /// 更新最後活動時間（只往後）
    fn touch_session(&self, hash: &[u8], now_ms: i64) -> StoreResult<()>;

    /// 結束一個工作階段；回傳是否有變更
    fn end_session(&self, hash: &[u8], reason: &str, by: &str, now_ms: i64) -> StoreResult<bool>;

    /// 以公開的 id 結束使用者的一個工作階段
    fn end_session_by_public_id(
        &self,
        user_id: i64,
        public_id: &str,
        reason: &str,
        by: &str,
        now_ms: i64,
    ) -> StoreResult<bool>;

    /// 結束使用者的所有工作階段（`except` 為保留的那一個）；回傳結束的數量。
    /// 已逾時但未標記結束的也一併標記（不計入回傳的數量）
    fn end_user_sessions(
        &self,
        user_id: i64,
        except: Option<&[u8]>,
        reason: &str,
        by: &str,
        now_ms: i64,
    ) -> StoreResult<usize>;

    /// 清除結束或逾時超過 30 天的工作階段紀錄
    fn purge_sessions(&self, now_ms: i64) -> StoreResult<usize>;
}
