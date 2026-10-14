//! 使用者、角色指派、工作階段、兩步驟驗證與目錄服務（審計 F6；設計包 `kairos/screens.md` S01、S16、S17、
//! S23 與 §24 #10；`kairos/API.md` 第 12 節）。
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`password`] | Argon2id 雜湊、密碼政策、產生一次性密碼 |
//! | [`totp`] | RFC 6238 TOTP 與復原碼 |
//! | [`secrets`] | 主控台金鑰（CSRF、TOTP 秘密的加密、復原碼的 HMAC） |
//! | [`store`] | `users`、`role_assignments`、`sessions`、`user_recovery_codes` 的讀寫 |
//! | [`ldap`] | LDAP／AD 的 bind 驗證與群組 |
//!
//! 角色與權限的定義在 [`crate::rbac`]；HTTP 端點在 `api::account`、`api::users`；CLI 在
//! [`crate::users_cli`]。

pub mod ldap;
pub mod password;
pub mod secrets;
pub mod store;
pub mod totp;

use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::sync::Mutex;

/// 工作階段的秘密：32 bytes 隨機值的 base64url（cookie 的值；資料庫只保存 SHA-256）
pub fn new_session_secret() -> Result<String, String> {
    let mut b = [0u8; 32];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| "無法取得隨機數（作業系統的 CSPRNG）".to_string())?;
    Ok(crate::api::http::b64url_encode(&b))
}

/// 公開的識別碼（16 個十六進位字元；工作階段清單、MFA 第二步的挑戰）
pub fn new_public_id() -> Result<String, String> {
    let mut b = [0u8; 8];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| "無法取得隨機數（作業系統的 CSPRNG）".to_string())?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// SHA-256
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// 不存在的帳號（與目錄服務帳號）的失敗計數，在記憶體中（設計包 S01：「訊息不透露帳號是否存在」：
/// 不存在的帳號同樣在連續失敗後顯示「帳號已鎖定」）。最多追蹤 [`MAX_TRACKED`] 個名稱
#[derive(Debug, Default)]
pub struct PhantomLockout {
    entries: Mutex<HashMap<String, (u32, i64)>>,
}

/// 追蹤的名稱上限
pub const MAX_TRACKED: usize = 10_000;

impl PhantomLockout {
    /// 鎖定到何時（目前鎖定中時）
    pub fn locked_until(&self, key: &str, now_ms: i64) -> Option<i64> {
        let e = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        e.get(&key.to_lowercase())
            .map(|(_, until)| *until)
            .filter(|u| *u > now_ms)
    }

    /// 失敗一次；達到門檻時鎖定並回傳鎖定到何時
    pub fn fail(&self, key: &str, threshold: u32, lock_ms: i64, now_ms: i64) -> Option<i64> {
        let mut e = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if e.len() >= MAX_TRACKED && !e.contains_key(&key.to_lowercase()) {
            // 清掉沒有鎖定的；仍然太多時清掉最早到期的
            e.retain(|_, (_, until)| *until > now_ms);
            if e.len() >= MAX_TRACKED
                && let Some(k) = e
                    .iter()
                    .min_by_key(|(_, (_, u))| *u)
                    .map(|(k, _)| k.clone())
            {
                e.remove(&k);
            }
        }
        let entry = e.entry(key.to_lowercase()).or_insert((0, 0));
        if entry.1 > now_ms {
            return Some(entry.1);
        }
        entry.0 += 1;
        if entry.0 >= threshold {
            entry.0 = 0;
            entry.1 = now_ms + lock_ms;
            return Some(entry.1);
        }
        None
    }

    pub fn succeed(&self, key: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key.to_lowercase());
    }

    /// 清掉已解鎖、且沒有失敗紀錄的
    pub fn prune(&self, now_ms: i64) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (n, until)| *until > now_ms || *n > 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_random() {
        let a = new_session_secret().unwrap();
        assert_eq!(a.len(), 43);
        assert_ne!(a, new_session_secret().unwrap());
        assert_eq!(new_public_id().unwrap().len(), 16);
        assert_eq!(sha256(b"abc")[..2], [0xba, 0x78]);
    }

    #[test]
    fn phantom_lockout_behaves_like_a_real_account() {
        let p = PhantomLockout::default();
        assert_eq!(p.fail("Ghost", 3, 1000, 0), None);
        assert_eq!(p.fail("ghost", 3, 1000, 0), None);
        assert_eq!(p.fail("GHOST", 3, 1000, 0), Some(1000));
        assert_eq!(p.locked_until("ghost", 500), Some(1000));
        assert_eq!(p.locked_until("ghost", 1000), None);
        // 鎖定期間的嘗試不延長
        assert_eq!(p.fail("ghost", 3, 1000, 600), Some(1000));
        p.succeed("ghost");
        assert_eq!(p.locked_until("ghost", 0), None);
        for i in 0..(MAX_TRACKED + 5) {
            p.fail(&format!("u{i}"), 3, 1000, 0);
        }
        assert!(p.entries.lock().unwrap().len() <= MAX_TRACKED);
        p.prune(2000);
    }
}
