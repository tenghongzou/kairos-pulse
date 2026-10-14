//! 本機帳號的密碼：Argon2id 雜湊（PHC 字串）、密碼政策、產生一次性的初始密碼。
//!
//! - 參數來自 `[accounts]`（預設 m=19 MiB、t=2、p=1）；驗證時使用雜湊字串中記錄的參數，
//!   參數調整後舊的雜湊仍可驗證，並在下次登入成功時以新參數重新雜湊（[`needs_rehash`]）。
//! - 比對由 `password-hash` 以常數時間進行；帳號不存在時仍以同樣的參數驗證一次（[`dummy_verify`]），
//!   回應時間不透露帳號是否存在。
//! - 鹽為 16 bytes，由 ring 的 CSPRNG 產生。密碼本身不出現在日誌、資料庫或任何回應。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use ring::rand::{SecureRandom, SystemRandom};
use std::sync::OnceLock;

use crate::config::AccountSettings;

/// 密碼的長度上限（字元）：避免以超長的輸入消耗雜湊的 CPU
pub const MAX_PASSWORD_CHARS: usize = 256;

/// Argon2id 的參數
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl From<&AccountSettings> for HashParams {
    fn from(a: &AccountSettings) -> Self {
        Self {
            memory_kib: a.argon2_memory_kib,
            iterations: a.argon2_iterations,
            parallelism: a.argon2_parallelism,
        }
    }
}

impl Default for HashParams {
    fn default() -> Self {
        HashParams::from(&AccountSettings::default())
    }
}

fn argon2(p: HashParams) -> Result<Argon2<'static>, String> {
    let params = Params::new(p.memory_kib, p.iterations, p.parallelism, None)
        .map_err(|e| format!("Argon2 參數無效：{e}"))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// 以 Argon2id 雜湊；回傳 PHC 字串（`$argon2id$v=19$m=…,t=…,p=…$<鹽>$<雜湊>`）
pub fn hash(password: &str, p: HashParams) -> Result<String, String> {
    let mut salt = [0u8; 16];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| "無法取得隨機數（作業系統的 CSPRNG）".to_string())?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
    argon2(p)?
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("無法雜湊密碼：{e}"))
}

/// 驗證密碼（只接受 Argon2id 的雜湊；以雜湊中記錄的參數計算，常數時間比對）
pub fn verify(password: &str, phc: &str) -> bool {
    if password.chars().count() > MAX_PASSWORD_CHARS {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    if parsed.algorithm.as_str() != "argon2id" {
        return false;
    }
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// 雜湊的參數與目前的設定不同（登入成功後以新參數重新雜湊）
pub fn needs_rehash(phc: &str, p: HashParams) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return true;
    };
    let Ok(params) = Params::try_from(&parsed) else {
        return true;
    };
    parsed.algorithm.as_str() != "argon2id"
        || params.m_cost() != p.memory_kib
        || params.t_cost() != p.iterations
        || params.p_cost() != p.parallelism
}

/// 帳號不存在（或沒有密碼）時，以同樣的參數驗證一次假的雜湊，讓回應時間相同
pub fn dummy_verify(password: &str, p: HashParams) {
    static DUMMY: OnceLock<std::sync::Mutex<Option<(HashParams, String)>>> = OnceLock::new();
    let cell = DUMMY.get_or_init(|| std::sync::Mutex::new(None));
    let phc = {
        let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
        match &*g {
            Some((params, h)) if *params == p => h.clone(),
            _ => {
                let h = hash("kairos-dummy-password", p).unwrap_or_default();
                *g = Some((p, h.clone()));
                h
            }
        }
    };
    let _ = verify(password, &phc);
}

/// 常見、容易被猜中的密碼（只做最基本的檢查；長度才是主要的防線）
const COMMON: [&str; 16] = [
    "password",
    "123456",
    "qwerty",
    "admin",
    "kairos",
    "letmein",
    "welcome",
    "iloveyou",
    "abc123",
    "111111",
    "passw0rd",
    "p@ssw0rd",
    "changeme",
    "administrator",
    "1q2w3e4r",
    "zaq12wsx",
];

/// 密碼政策：長度、不含控制字元、不包含帳號、不是單一字元重複、不是常見密碼的變形
pub fn check_policy(password: &str, username: &str, min_len: usize) -> Result<(), String> {
    let n = password.chars().count();
    if n < min_len {
        return Err(format!("密碼至少需要 {min_len} 個字元（目前 {n} 個）"));
    }
    if n > MAX_PASSWORD_CHARS {
        return Err(format!("密碼最多 {MAX_PASSWORD_CHARS} 個字元"));
    }
    if password.chars().any(char::is_control) {
        return Err("密碼不可含控制字元".to_string());
    }
    let lower = password.to_lowercase();
    let user = username.trim().to_lowercase();
    if user.chars().count() >= 3 && lower.contains(&user) {
        return Err("密碼不可包含帳號名稱".to_string());
    }
    let mut chars = password.chars();
    if let Some(first) = chars.next()
        && chars.all(|c| c == first)
    {
        return Err("密碼不可是同一個字元重複".to_string());
    }
    let stripped: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '@')
        .collect();
    if COMMON.iter().any(|c| {
        stripped == *c
            || (stripped.starts_with(c) && stripped[c.len()..].chars().all(|x| x.is_ascii_digit()))
    }) {
        return Err("密碼太常見，容易被猜中".to_string());
    }
    Ok(())
}

/// 產生的初始或重設密碼的字母表（去除容易混淆的 0／O、1／l／I）
const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 產生一次性的密碼：4 組各 5 個字元（約 116 位元），以 `-` 分隔，例如 `k7Qm2-pXr9a-…`
pub fn generate() -> Result<String, String> {
    let rng = SystemRandom::new();
    let mut out = String::with_capacity(23);
    for group in 0..4 {
        if group > 0 {
            out.push('-');
        }
        let mut i = 0;
        while i < 5 {
            let mut b = [0u8; 1];
            rng.fill(&mut b)
                .map_err(|_| "無法取得隨機數（作業系統的 CSPRNG）".to_string())?;
            // 拒絕取樣：避免模數偏差
            let limit = 256 - (256 % ALPHABET.len());
            if (b[0] as usize) < limit {
                out.push(ALPHABET[b[0] as usize % ALPHABET.len()] as char);
                i += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 測試用的低成本參數（正式的預設值另外測一次）
    fn cheap() -> HashParams {
        HashParams {
            memory_kib: 8,
            iterations: 1,
            parallelism: 1,
        }
    }

    #[test]
    fn hash_and_verify_with_argon2id() {
        let h = hash("correct horse battery", cheap()).unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=8,t=1,p=1$"), "{h}");
        assert!(!h.contains("correct horse"));
        assert!(verify("correct horse battery", &h));
        assert!(!verify("correct horse batterY", &h));
        assert!(!verify("", &h));
        // 同一個密碼每次的鹽不同
        assert_ne!(hash("correct horse battery", cheap()).unwrap(), h);
        // 只接受 Argon2id
        let argon2i = h.replace("$argon2id$", "$argon2i$");
        assert!(!verify("correct horse battery", &argon2i));
        assert!(!verify("x", "not a phc string"));
        assert!(!verify(&"a".repeat(MAX_PASSWORD_CHARS + 1), &h));
        // 參數改變時需要重新雜湊
        assert!(!needs_rehash(&h, cheap()));
        assert!(needs_rehash(&h, HashParams::default()));
        assert!(needs_rehash("garbage", cheap()));
        dummy_verify("x", cheap());
    }

    #[test]
    fn default_parameters_follow_owasp() {
        let p = HashParams::default();
        assert_eq!((p.memory_kib, p.iterations, p.parallelism), (19_456, 2, 1));
        let h = hash("Tr0ub4dor&3-long", p).unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{h}");
        assert!(verify("Tr0ub4dor&3-long", &h));
    }

    #[test]
    fn password_policy() {
        assert!(check_policy("correct horse battery", "admin", 12).is_ok());
        assert!(check_policy("short", "admin", 12).is_err());
        assert!(check_policy(&"a".repeat(20), "admin", 12).is_err());
        assert!(check_policy("my-admin-password", "admin", 12).is_err());
        assert!(check_policy("Password12345", "bob", 12).is_err());
        assert!(check_policy("P@ssw0rd2026!!", "bob", 12).is_err());
        assert!(check_policy("tab\there-and-more", "bob", 12).is_err());
        assert!(check_policy(&"ab".repeat(200), "bob", 12).is_err());
        // 帳號太短時不做「包含帳號」的檢查
        assert!(check_policy("abcdefghijklmn", "ab", 12).is_ok());
    }

    #[test]
    fn generated_passwords_satisfy_the_policy() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 23);
        assert_eq!(a.split('-').count(), 4);
        assert!(check_policy(&a, "admin", 12).is_ok());
        assert!(a.bytes().all(|c| c == b'-' || ALPHABET.contains(&c)));
    }
}
